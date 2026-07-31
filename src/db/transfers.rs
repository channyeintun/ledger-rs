//! The transfer path: the only code in the system that creates money movements.
//!
//! # Concurrency strategy (invariant #2: no account balance ever goes negative)
//!
//! Three mechanisms stack, each catching what the others structurally cannot.
//!
//! ## 1. By construction — `CHECK (allows_negative_balance OR balance >= 0)`
//!
//! `accounts.balance` is a materialized cache of the entry log, and it carries
//! a `CHECK` constraint. A negative balance on an ordinary account is therefore
//! not a state the database can hold: no bug in this file, no future caller, and
//! no manual `psql` session can produce one. This is the authority. The
//! application-level check below exists to produce a *good error message*, not
//! to be the enforcement.
//!
//! Note the deliberate scope limit, spelled out in the migration: a hard `CHECK`
//! is only safe because this ledger is closed. Every balance change originates
//! from a transfer authorized inside the same database transaction, so nothing
//! external can force a negative position on us after the fact. A system that
//! ingests settlements or chargebacks must instead model overdrafts as
//! representable-but-monitored, because code that cannot represent the state it
//! is forced into will either abort mid-flow or clamp to zero — and clamping
//! mints money.
//!
//! ## 2. Row locks in a globally consistent order
//!
//! A `CHECK` alone would turn concurrent drains into a storm of aborted
//! transactions. Before touching balances, both accounts are locked with
//! `SELECT ... FOR UPDATE`, **in ascending UUID order**.
//!
//! * *Why lock at all:* the balance check and the debit it authorizes must be
//!   one atomic step. Reading a balance and then updating it in a separate
//!   statement is the classic overdraft race — two requests both read 100, both
//!   conclude 100 >= 60, and one of them writes a negative balance. Holding the
//!   row lock across both makes the pair linearizable.
//! * *Why ascending UUID order:* a transfer A→B and a concurrent transfer B→A
//!   would otherwise grab the two locks in opposite orders and deadlock. Sorting
//!   the ids gives every transaction in the system the same lock sequence, so
//!   they queue instead. This is why the locks are taken as two explicit,
//!   separately ordered statements rather than one `WHERE id = ANY(...)`: it
//!   makes the acquisition order a property of this code rather than of the
//!   query planner's chosen row order.
//! * *Why `READ COMMITTED` suffices:* the update is written as
//!   `balance = balance - $1`, not `balance = $precomputed`. Once the lock is
//!   granted, Postgres re-reads the latest committed row version and re-applies
//!   the expression to it, so no update is lost. `SERIALIZABLE` would also be
//!   correct but would turn the 100-parallel-drain test into a retry storm,
//!   since every transfer conflicts with every other on the same account.
//!
//! ## 3. Post-factum — `ledger_check_invariants()`
//!
//! The sweep in the migration re-derives every balance from the entry log and
//! compares it with the cache. It is what catches drift introduced by anything
//! that bypassed this file entirely.
//!
//! # Idempotency (invariant #4: replays never create duplicate entries)
//!
//! The `UNIQUE` index on `transactions.idempotency_key` is the serialization
//! point, and it is claimed **before** any balance is touched. Two concurrent
//! requests carrying the same key both attempt the insert; one wins, and the
//! other blocks on the index until the winner commits and then fails with
//! `23505`. The loser never reaches the entry inserts, so duplicate entries are
//! not merely unlikely, they are unreachable. A separate read-then-insert
//! "check if the key exists first" barrier would be exactly the race this avoids.
//!
//! ## A rejected transfer releases its key
//!
//! The key is claimed inside the same database transaction that moves the
//! money, so a rejection — insufficient funds, unknown account, a transient
//! failure — rolls the claim back with everything else. The key is then free,
//! and the client may retry it.
//!
//! This is a deliberate choice with a real trade-off, not an accident:
//!
//! * *For:* a transient failure must never permanently poison a key. If the
//!   claim survived the rollback, a database blip would leave the caller unable
//!   to complete a payment it is entitled to make, with no way to distinguish
//!   "already done" from "never happened".
//! * *Against:* it means the same key can produce different outcomes at
//!   different times. A caller that is rejected for insufficient funds, and
//!   retries the same key after the account is topped up, moves money — where
//!   a system that persisted the failure would keep returning the original
//!   error. (Stripe, for instance, persists API-level errors against the key.)
//!
//! Callers that need "this exact attempt failed, permanently" should mint a new
//! key per attempt rather than reusing one across a funding event.

use std::collections::HashMap;

use rust_decimal::Decimal;
use sqlx::{PgPool, Postgres, Transaction as SqlxTransaction};
use uuid::Uuid;

use crate::db::{accounts, transactions};
use crate::domain::{Currency, Direction, TransactionDetail, TransferIntent, TransferOutcome};
use crate::error::{LedgerError, Result, is_idempotency_key_collision, is_overdraft_violation};

pub struct TransferResult {
    pub outcome: TransferOutcome,
    pub detail: TransactionDetail,
}

/// Moves `intent.amount` from one account to another, exactly once per
/// idempotency key.
pub async fn execute(
    pool: &PgPool,
    idempotency_key: &str,
    intent: &TransferIntent,
) -> Result<TransferResult> {
    let request_hash = intent.request_hash();

    // Fast path for the common replay: a key we have already seen never
    // reaches the write path at all. This is an optimisation, not the
    // idempotency barrier — the barrier is the unique index below, which is
    // what makes the *concurrent* case safe.
    if let Some(record) = transactions::find_by_idempotency_key(pool, idempotency_key).await? {
        return replay(pool, idempotency_key, record, &request_hash).await;
    }

    if intent.from_account_id == intent.to_account_id {
        return Err(LedgerError::SelfTransfer);
    }
    if !intent.amount.is_positive() {
        return Err(LedgerError::NonPositiveAmount);
    }

    let mut tx = pool.begin().await?;

    let transaction_id = Uuid::new_v4();
    let claim = sqlx::query(
        "INSERT INTO transactions (id, idempotency_key, request_hash, description)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(transaction_id)
    .bind(idempotency_key)
    .bind(request_hash.as_slice())
    .bind(&intent.description)
    .execute(&mut *tx)
    .await;

    if let Err(err) = claim {
        if is_idempotency_key_collision(&err) {
            // A concurrent request won this key. Our transaction is already
            // aborted by the violation, so it must be discarded before we can
            // read the winner's committed result on a fresh connection.
            tx.rollback().await?;

            let record = transactions::find_by_idempotency_key(pool, idempotency_key)
                .await?
                .ok_or_else(|| {
                    // Unreachable: transactions are never deleted, so a 23505
                    // means the row is committed and visible. Surfaced rather
                    // than unwrapped so a schema change that breaks this
                    // reasoning fails loudly instead of silently double-spending.
                    LedgerError::Validation(
                        "idempotency key collided but no transaction was found".into(),
                    )
                })?;

            return replay(pool, idempotency_key, record, &request_hash).await;
        }
        return Err(err.into());
    }

    // --- Lock both accounts in ascending UUID order. See the module docs. ---
    let mut lock_order = [intent.from_account_id, intent.to_account_id];
    lock_order.sort_unstable();

    let mut locked = HashMap::with_capacity(2);
    for id in lock_order {
        let row = accounts::fetch_for_update(&mut *tx, id)
            .await?
            .ok_or(LedgerError::AccountNotFound(id))?;
        locked.insert(id, row);
    }

    let from = &locked[&intent.from_account_id];
    let to = &locked[&intent.to_account_id];
    let currency = intent.amount.currency();

    for account in [from, to] {
        if &account.currency != currency {
            return Err(LedgerError::AccountCurrencyMismatch {
                account_id: account.id,
                account_currency: account.currency.to_string(),
                transfer_currency: currency.to_string(),
            });
        }
    }

    let amount = intent.amount.amount();

    // Runtime rung. The `CHECK` constraint is the real enforcement; checking
    // here under the lock lets us return a precise 422 rather than a generic
    // constraint failure, and avoids burning a transaction abort on the
    // ordinary "not enough money" case.
    if !from.allows_negative_balance && from.balance < amount {
        return Err(LedgerError::InsufficientFunds {
            account_id: from.id,
        });
    }

    // Debit-positive convention: the sender is credited (balance falls), the
    // receiver is debited (balance rises).
    adjust_balance(&mut tx, from.id, -amount).await?;
    adjust_balance(&mut tx, to.id, amount).await?;

    insert_entry(
        &mut tx,
        transaction_id,
        from.id,
        Direction::Credit,
        amount,
        currency,
    )
    .await?;
    insert_entry(
        &mut tx,
        transaction_id,
        to.id,
        Direction::Debit,
        amount,
        currency,
    )
    .await?;

    // COMMIT is where the deferred constraint trigger checks invariant #1: at
    // this point every entry of the transaction is present, and the database
    // refuses the whole thing if debits and credits do not net to zero.
    if let Err(err) = tx.commit().await {
        if is_overdraft_violation(&err) {
            return Err(LedgerError::InsufficientFunds {
                account_id: from.id,
            });
        }
        return Err(err.into());
    }

    Ok(TransferResult {
        outcome: TransferOutcome::Created,
        detail: transactions::get(pool, transaction_id).await?,
    })
}

/// Same key, same payload replays the original result; same key, different
/// payload is a conflict. Comparing a hash rather than the payload itself keeps
/// the stored footprint fixed and avoids re-parsing an old request body.
async fn replay(
    pool: &PgPool,
    idempotency_key: &str,
    record: transactions::IdempotencyRecord,
    request_hash: &[u8; 32],
) -> Result<TransferResult> {
    if record.request_hash != request_hash.as_slice() {
        return Err(LedgerError::IdempotencyKeyConflict {
            key: idempotency_key.to_owned(),
        });
    }

    Ok(TransferResult {
        outcome: TransferOutcome::Replayed,
        detail: transactions::get(pool, record.transaction_id).await?,
    })
}

/// Applies a signed delta to a locked account's balance.
///
/// Written as `balance = balance + $1` rather than `balance = $precomputed` so
/// that Postgres re-applies the delta to the freshest committed row version
/// after the lock is granted. That is what makes `READ COMMITTED` sufficient.
async fn adjust_balance(
    tx: &mut SqlxTransaction<'_, Postgres>,
    account_id: Uuid,
    delta: Decimal,
) -> Result<()> {
    sqlx::query("UPDATE accounts SET balance = balance + $1 WHERE id = $2")
        .bind(delta)
        .bind(account_id)
        .execute(&mut **tx)
        .await
        .map_err(|err| {
            if is_overdraft_violation(&err) {
                LedgerError::InsufficientFunds { account_id }
            } else {
                LedgerError::Database(err)
            }
        })?;
    Ok(())
}

async fn insert_entry(
    tx: &mut SqlxTransaction<'_, Postgres>,
    transaction_id: Uuid,
    account_id: Uuid,
    direction: Direction,
    amount: Decimal,
    currency: &Currency,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO entries (id, transaction_id, account_id, direction, amount, currency)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(transaction_id)
    .bind(account_id)
    .bind(direction)
    .bind(amount)
    .bind(currency)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
