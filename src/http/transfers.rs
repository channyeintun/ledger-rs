//! `POST /transfers`.
//!
//! Requires an `Idempotency-Key` header. Same key with the same payload replays
//! the original result with `200`; same key with a different payload is a `409`.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use crate::db;
use crate::domain::{Currency, Money, TransactionDetail, TransferIntent, TransferOutcome};
use crate::error::{LedgerError, Result};
use crate::http::AppState;

/// Upper bound on the header value, so a client cannot pin unbounded storage
/// with one request.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// Upper bound on a transfer description. The ledger is append-only, so
/// anything accepted here is stored forever and can never be trimmed — an
/// unbounded field is a permanent, unrecoverable commitment to whatever a
/// client sent.
const MAX_DESCRIPTION_LEN: usize = 512;

#[derive(Debug, Deserialize)]
pub struct TransferRequest {
    pub from_account_id: Uuid,
    pub to_account_id: Uuid,
    /// A JSON **string**, not a number: a bare number would be parsed as a
    /// double by most clients and silently lose precision before it ever
    /// reaches this service.
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub currency: Currency,
    #[serde(default)]
    pub description: String,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: std::result::Result<Json<TransferRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<TransactionDetail>)> {
    let idempotency_key = extract_idempotency_key(&headers)?;
    let Json(request) = payload.map_err(|err| LedgerError::Validation(err.body_text()))?;

    // Rejected here rather than at the database, so a client sending an amount
    // finer than the ledger's precision is told so instead of having it
    // silently rounded on INSERT.
    let amount = Money::new(request.amount, request.currency)?;

    if request.description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(LedgerError::Validation(format!(
            "description must be at most {MAX_DESCRIPTION_LEN} characters"
        )));
    }

    let intent = TransferIntent {
        from_account_id: request.from_account_id,
        to_account_id: request.to_account_id,
        amount,
        description: request.description,
    };

    let result = db::transfers::execute(&state.pool, &idempotency_key, &intent).await?;

    let status = match result.outcome {
        TransferOutcome::Created => StatusCode::CREATED,
        TransferOutcome::Replayed => StatusCode::OK,
    };

    Ok((status, Json(result.detail)))
}

fn extract_idempotency_key(headers: &HeaderMap) -> Result<String> {
    let raw = headers
        .get("idempotency-key")
        .ok_or(LedgerError::MissingIdempotencyKey)?;

    let key = raw
        .to_str()
        .map_err(|_| LedgerError::Validation("Idempotency-Key must be valid UTF-8".into()))?
        .trim();

    if key.is_empty() {
        return Err(LedgerError::MissingIdempotencyKey);
    }
    if key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(LedgerError::Validation(format!(
            "Idempotency-Key must be at most {MAX_IDEMPOTENCY_KEY_LEN} bytes"
        )));
    }

    Ok(key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn missing_or_blank_idempotency_key_is_rejected() {
        let empty = HeaderMap::new();
        assert!(matches!(
            extract_idempotency_key(&empty),
            Err(LedgerError::MissingIdempotencyKey)
        ));

        let mut blank = HeaderMap::new();
        blank.insert("idempotency-key", HeaderValue::from_static("   "));
        assert!(matches!(
            extract_idempotency_key(&blank),
            Err(LedgerError::MissingIdempotencyKey)
        ));
    }

    #[test]
    fn idempotency_key_is_trimmed_and_length_bounded() {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("  abc  "));
        assert_eq!(extract_idempotency_key(&headers).unwrap(), "abc");

        let long = "k".repeat(MAX_IDEMPOTENCY_KEY_LEN + 1);
        headers.insert("idempotency-key", HeaderValue::from_str(&long).unwrap());
        assert!(matches!(
            extract_idempotency_key(&headers),
            Err(LedgerError::Validation(_))
        ));
    }

    #[test]
    fn amount_must_be_a_json_string_not_a_number() {
        let with_number = r#"{"from_account_id":"00000000-0000-0000-0000-000000000001",
            "to_account_id":"00000000-0000-0000-0000-000000000002",
            "amount":10.25,"currency":"USD"}"#;
        assert!(
            serde_json::from_str::<TransferRequest>(with_number).is_err(),
            "a bare JSON number must be rejected; it is where precision is lost"
        );

        let with_string = r#"{"from_account_id":"00000000-0000-0000-0000-000000000001",
            "to_account_id":"00000000-0000-0000-0000-000000000002",
            "amount":"10.25","currency":"USD"}"#;
        let parsed: TransferRequest = serde_json::from_str(with_string).unwrap();
        assert_eq!(parsed.amount.to_string(), "10.25");
    }
}
