//! HTTP edge. Handlers translate between JSON and the domain, and do nothing
//! else — every rule that matters lives in `db` or in the schema.

pub mod accounts;
pub mod health;
pub mod transactions;
pub mod transfers;

use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tower_http::LatencyUnit;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// The application with its full production middleware stack.
pub fn app(pool: PgPool, config: &Config) -> Router {
    // Layers apply outermost-first on the way in. The ordering here is load
    // bearing:
    //
    // 1. request id is set first, so every log line below it — including a
    //    panic or a timeout — can be correlated with the client's request.
    // 2. sensitive headers are marked before tracing, so they are never
    //    recorded even by the layer that exists to record things.
    // 3. catch-panic sits above the timeout so a panic still produces a
    //    response rather than a dropped connection.
    // 4. the body limit sits closest to the handler; nothing above it needs to
    //    read the body.
    router(pool)
        .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout,
        ))
        .layer(CatchPanicLayer::custom(handle_panic))
        .layer(
            TraceLayer::new_for_http().on_response(
                DefaultOnResponse::new()
                    .level(Level::INFO)
                    .latency_unit(LatencyUnit::Millis),
            ),
        )
        .layer(SetSensitiveRequestHeadersLayer::new([
            header::AUTHORIZATION,
            header::COOKIE,
            header::PROXY_AUTHORIZATION,
        ]))
        .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER))
        .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
}

/// Routes only, with no middleware. Useful for tests that want to exercise a
/// handler without the stack around it.
pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/accounts", post(accounts::create))
        .route("/accounts/{id}", get(accounts::get))
        .route("/transfers", post(transfers::create))
        .route("/transactions/{id}", get(transactions::get))
        .with_state(AppState { pool })
}

/// Turns a panic into the same error envelope every other failure uses.
///
/// A panic mid-transfer is always a bug, but the database transaction is rolled
/// back by the connection being dropped, so the ledger is not left inconsistent
/// — the caller just needs a response they can parse and retry against.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = if let Some(s) = err.downcast_ref::<String>() {
        s.as_str()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        s
    } else {
        "unknown panic"
    };

    tracing::error!(panic = detail, "handler panicked");

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({
            "error": { "code": "internal_error", "message": "an internal error occurred" }
        })),
    )
        .into_response()
}

/// Time a shutdown signal gives in-flight requests before the process exits.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
