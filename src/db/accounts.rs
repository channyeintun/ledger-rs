//! Account reads and writes.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::domain::{Account, Currency, Money};
use crate::error::{LedgerError, Result};

/// The `accounts` row as stored. Kept private so that `balance` and `currency`
/// can only leave this module recombined into a [`Money`], never as a bare
/// `Decimal` that could be added to an amount in another currency.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct AccountRow {
    pub id: Uuid,
    pub name: String,
    pub currency: Currency,
    pub balance: Decimal,
    pub allows_negative_balance: bool,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<AccountRow> for Account {
    type Error = LedgerError;

    fn try_from(row: AccountRow) -> Result<Account> {
        Ok(Account {
            id: row.id,
            name: row.name,
            balance: Money::new(row.balance, row.currency)?,
            allows_negative_balance: row.allows_negative_balance,
            created_at: row.created_at,
        })
    }
}

pub async fn create(
    pool: &PgPool,
    name: &str,
    currency: &Currency,
    allows_negative_balance: bool,
) -> Result<Account> {
    let row = sqlx::query_as::<_, AccountRow>(
        "INSERT INTO accounts (id, name, currency, allows_negative_balance)
         VALUES ($1, $2, $3, $4)
         RETURNING id, name, currency, balance, allows_negative_balance, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(name)
    .bind(currency)
    .bind(allows_negative_balance)
    .fetch_one(pool)
    .await
    .map_err(|err| match crate::error::constraint_failure(&err) {
        Some((code, _)) if code == crate::error::sqlstate::CHECK_VIOLATION => {
            LedgerError::Validation("account name must be 1-255 characters".into())
        }
        _ => LedgerError::Database(err),
    })?;

    row.try_into()
}

/// Reads an account and its current balance.
///
/// The balance comes from the materialized `accounts.balance` column rather
/// than from a `SUM` over `entries`. It is derived from the entry log — the
/// transfer path moves both in the same database transaction — but materialized
/// so that this read is O(1) instead of O(entries) and so that invariant #2 can
/// be a `CHECK` constraint. `db::check_invariants` proves the two never
/// diverge; see the "Concurrency strategy" section of the README.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<Account> {
    let row = fetch(pool, id)
        .await?
        .ok_or(LedgerError::AccountNotFound(id))?;
    row.try_into()
}

pub(crate) async fn fetch<'e, E: PgExecutor<'e>>(
    executor: E,
    id: Uuid,
) -> Result<Option<AccountRow>> {
    Ok(sqlx::query_as::<_, AccountRow>(
        "SELECT id, name, currency, balance, allows_negative_balance, created_at
         FROM accounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?)
}

/// Reads an account **and takes its row lock**, blocking any concurrent
/// transfer touching the same account until this transaction ends.
///
/// Callers must acquire these locks in a globally consistent order — see
/// [`crate::db::transfers::execute`].
pub(crate) async fn fetch_for_update<'e, E: PgExecutor<'e>>(
    executor: E,
    id: Uuid,
) -> Result<Option<AccountRow>> {
    Ok(sqlx::query_as::<_, AccountRow>(
        "SELECT id, name, currency, balance, allows_negative_balance, created_at
         FROM accounts WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?)
}
