//! Transaction and entry reads.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::domain::{Currency, Direction, Entry, Money, Transaction, TransactionDetail};
use crate::error::{LedgerError, Result};

#[derive(Debug, Clone, sqlx::FromRow)]
struct TransactionRow {
    id: Uuid,
    idempotency_key: String,
    description: String,
    created_at: DateTime<Utc>,
}

impl From<TransactionRow> for Transaction {
    fn from(row: TransactionRow) -> Transaction {
        Transaction {
            id: row.id,
            idempotency_key: row.idempotency_key,
            description: row.description,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EntryRow {
    id: Uuid,
    transaction_id: Uuid,
    account_id: Uuid,
    direction: Direction,
    amount: Decimal,
    currency: Currency,
    created_at: DateTime<Utc>,
}

impl TryFrom<EntryRow> for Entry {
    type Error = LedgerError;

    fn try_from(row: EntryRow) -> Result<Entry> {
        Ok(Entry {
            id: row.id,
            transaction_id: row.transaction_id,
            account_id: row.account_id,
            direction: row.direction,
            amount: Money::new(row.amount, row.currency)?,
            created_at: row.created_at,
        })
    }
}

/// A transaction's identity as recorded for idempotency purposes.
#[derive(Debug, Clone)]
pub(crate) struct IdempotencyRecord {
    pub transaction_id: Uuid,
    pub request_hash: Vec<u8>,
}

pub(crate) async fn find_by_idempotency_key<'e, E: PgExecutor<'e>>(
    executor: E,
    key: &str,
) -> Result<Option<IdempotencyRecord>> {
    let row: Option<(Uuid, Vec<u8>)> =
        sqlx::query_as("SELECT id, request_hash FROM transactions WHERE idempotency_key = $1")
            .bind(key)
            .fetch_optional(executor)
            .await?;

    Ok(row.map(|(transaction_id, request_hash)| IdempotencyRecord {
        transaction_id,
        request_hash,
    }))
}

/// Loads a transaction together with every entry that belongs to it.
pub async fn get(pool: &PgPool, id: Uuid) -> Result<TransactionDetail> {
    let transaction: TransactionRow = sqlx::query_as(
        "SELECT id, idempotency_key, description, created_at
         FROM transactions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(LedgerError::TransactionNotFound(id))?;

    // Deterministic order so that a replayed response is byte-identical to the
    // original: an idempotent replay that reorders its own entries would defeat
    // the point of returning "the original result".
    let entry_rows: Vec<EntryRow> = sqlx::query_as(
        "SELECT id, transaction_id, account_id, direction, amount, currency, created_at
         FROM entries WHERE transaction_id = $1
         ORDER BY created_at, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let entries = entry_rows
        .into_iter()
        .map(Entry::try_from)
        .collect::<Result<Vec<_>>>()?;

    Ok(TransactionDetail {
        transaction: transaction.into(),
        entries,
    })
}
