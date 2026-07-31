//! Liveness and readiness.
//!
//! Kept distinct because they answer different questions and an orchestrator
//! reacts to them differently. Conflating the two is a classic way to turn a
//! brief database blip into a rolling restart of every healthy replica.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::http::AppState;

/// "This process is running." Deliberately touches nothing else: if this fails,
/// the answer is to restart the process, and that must not depend on the
/// database being reachable.
pub async fn live() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// "This process can serve traffic." Requires a working connection, since every
/// endpoint needs one. Returns 503 so a load balancer sheds traffic instead of
/// letting requests pile up against a pool that cannot serve them.
pub async fn ready(State(state): State<AppState>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "readiness probe failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable", "reason": "database unreachable" })),
            )
                .into_response()
        }
    }
}
