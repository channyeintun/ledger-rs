//! `POST /accounts` and `GET /accounts/{id}`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use crate::db;
use crate::domain::{Account, Currency};
use crate::error::{LedgerError, Result};
use crate::http::AppState;

/// Matches the `char_length(name) BETWEEN 1 AND 255` check in the schema.
const MAX_NAME_LEN: usize = 255;

#[derive(Debug, Deserialize)]
pub struct CreateAccountRequest {
    pub name: String,
    /// Validated against the ISO 4217 controlled set during deserialization.
    pub currency: Currency,
    /// Marks a funding/equity account, the only kind permitted to hold a
    /// negative (credit-normal) position. Defaults to a customer account, so
    /// the unsafe case is never the one you get by forgetting a field.
    #[serde(default)]
    pub allows_negative_balance: bool,
}

pub async fn create(
    State(state): State<AppState>,
    payload: std::result::Result<Json<CreateAccountRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Account>)> {
    let Json(request) = payload.map_err(|err| LedgerError::Validation(err.body_text()))?;

    let name = request.name.trim();
    if name.is_empty() {
        return Err(LedgerError::Validation("name must not be empty".into()));
    }
    // The database `CHECK` counts characters, so validate the same unit here
    // rather than bytes — otherwise a name of multi-byte characters passes this
    // check and is rejected one layer down with a worse message.
    if name.chars().count() > MAX_NAME_LEN {
        return Err(LedgerError::Validation(format!(
            "name must be at most {MAX_NAME_LEN} characters"
        )));
    }

    let account = db::accounts::create(
        &state.pool,
        name,
        &request.currency,
        request.allows_negative_balance,
    )
    .await?;

    Ok((StatusCode::CREATED, Json(account)))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<Account>> {
    Ok(Json(db::accounts::get(&state.pool, id).await?))
}
