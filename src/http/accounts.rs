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
