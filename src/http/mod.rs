//! HTTP edge. Handlers translate between JSON and the domain, and do nothing
//! else — every rule that matters lives in `db` or in the schema.

pub mod accounts;
pub mod transactions;
pub mod transfers;

use axum::Router;
use axum::routing::{get, post};
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/accounts", post(accounts::create))
        .route("/accounts/{id}", get(accounts::get))
        .route("/transfers", post(transfers::create))
        .route("/transactions/{id}", get(transactions::get))
        .with_state(AppState { pool })
}
