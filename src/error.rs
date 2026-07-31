//! The error taxonomy, and its single mapping onto HTTP.
//!
//! Every failure the ledger can produce is named here. Nothing maps to 500
//! except a genuine bug or an unavailable database — in particular, a rejected
//! transfer is a *typed* outcome, never an opaque internal error, because
//! "insufficient funds" and "the database fell over" demand different
//! behaviour from a caller retrying a payment.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::MoneyError;

pub type Result<T> = std::result::Result<T, LedgerError>;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("account {0} does not exist")]
    AccountNotFound(Uuid),

    #[error("transaction {0} does not exist")]
    TransactionNotFound(Uuid),

    #[error("account {account_id} has insufficient funds for this transfer")]
    InsufficientFunds { account_id: Uuid },

    #[error(
        "account {account_id} is denominated in {account_currency}, but the transfer is in {transfer_currency}"
    )]
    AccountCurrencyMismatch {
        account_id: Uuid,
        account_currency: String,
        transfer_currency: String,
    },

    #[error("a transfer must move money between two different accounts")]
    SelfTransfer,

    #[error("transfer amount must be greater than zero")]
    NonPositiveAmount,

    #[error("the Idempotency-Key header is required for this endpoint")]
    MissingIdempotencyKey,

    #[error("idempotency key '{key}' was already used with a different payload")]
    IdempotencyKeyConflict { key: String },

    #[error("{0}")]
    Validation(String),

    #[error(transparent)]
    Money(#[from] MoneyError),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl LedgerError {
    /// Stable, machine-readable discriminant. Clients branch on this, never on
    /// the human-readable message.
    pub fn code(&self) -> &'static str {
        match self {
            LedgerError::AccountNotFound(_) => "account_not_found",
            LedgerError::TransactionNotFound(_) => "transaction_not_found",
            LedgerError::InsufficientFunds { .. } => "insufficient_funds",
            LedgerError::AccountCurrencyMismatch { .. } => "currency_mismatch",
            LedgerError::SelfTransfer => "self_transfer",
            LedgerError::NonPositiveAmount => "non_positive_amount",
            LedgerError::MissingIdempotencyKey => "missing_idempotency_key",
            LedgerError::IdempotencyKeyConflict { .. } => "idempotency_key_conflict",
            LedgerError::Validation(_) => "validation_failed",
            LedgerError::Money(_) => "invalid_amount",
            LedgerError::Database(_) => "internal_error",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            LedgerError::AccountNotFound(_) | LedgerError::TransactionNotFound(_) => {
                StatusCode::NOT_FOUND
            }
            LedgerError::MissingIdempotencyKey => StatusCode::BAD_REQUEST,
            LedgerError::IdempotencyKeyConflict { .. } => StatusCode::CONFLICT,
            LedgerError::InsufficientFunds { .. }
            | LedgerError::AccountCurrencyMismatch { .. }
            | LedgerError::SelfTransfer
            | LedgerError::NonPositiveAmount
            | LedgerError::Validation(_)
            | LedgerError::Money(_) => StatusCode::UNPROCESSABLE_ENTITY,
            LedgerError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for LedgerError {
    fn into_response(self) -> Response {
        let status = self.status();

        // A database error may carry details about our own schema. Log it in
        // full, return only the discriminant.
        let message = match &self {
            LedgerError::Database(err) => {
                tracing::error!(error = %err, "unhandled database error");
                "an internal error occurred".to_string()
            }
            other => other.to_string(),
        };

        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message,
            },
        };

        (status, Json(body)).into_response()
    }
}

/// Postgres SQLSTATE codes the ledger interprets rather than passes through.
pub mod sqlstate {
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const RESTRICT_VIOLATION: &str = "23001";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
}

/// Inspects a `sqlx` error for a Postgres constraint failure.
///
/// Returns `(sqlstate, constraint_name)`. `RAISE ... USING CONSTRAINT = '...'`
/// in the migration is what makes the constraint name available for the
/// trigger-enforced invariants, not just the declarative ones.
pub fn constraint_failure(err: &sqlx::Error) -> Option<(String, Option<String>)> {
    let db_err = match err {
        sqlx::Error::Database(db_err) => db_err,
        _ => return None,
    };
    let code = db_err.code()?.into_owned();
    let constraint = db_err.constraint().map(str::to_owned);
    Some((code, constraint))
}

/// True when `err` is the unique-index collision on the idempotency key — the
/// signal that a concurrent request won the race for this key.
pub fn is_idempotency_key_collision(err: &sqlx::Error) -> bool {
    matches!(
        constraint_failure(err),
        Some((code, Some(constraint)))
            if code == sqlstate::UNIQUE_VIOLATION
                && constraint == "transactions_idempotency_key_key"
    )
}

/// True when `err` is the non-negative balance CHECK — the database refusing
/// an overdraft that the application-level check somehow let through.
pub fn is_overdraft_violation(err: &sqlx::Error) -> bool {
    matches!(
        constraint_failure(err),
        Some((code, Some(constraint)))
            if code == sqlstate::CHECK_VIOLATION
                && constraint == "accounts_balance_non_negative"
    )
}
