//! `GET /transactions/{id}`.

use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use crate::db;
use crate::domain::TransactionDetail;
use crate::error::Result;
use crate::http::AppState;

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<TransactionDetail>> {
    Ok(Json(db::transactions::get(&state.pool, id).await?))
}
