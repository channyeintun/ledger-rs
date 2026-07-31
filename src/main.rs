//! Binary entrypoint: configuration, pool, migrations, server.

use anyhow::Context;
use ledger_rs::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env()?;
    tracing::info!(
        database = %config.redacted_database_url(),
        bind = %config.bind_addr,
        max_connections = config.pool.max_connections,
        "starting ledger-rs"
    );

    let pool = ledger_rs::db::connect(&config.database_url, config.pool.clone())
        .await
        .context("failed to connect to the database")?;

    // sqlx takes a Postgres advisory lock for the duration, so running this on
    // every replica of a rolling deploy is safe: the others wait, then find
    // nothing to do.
    ledger_rs::db::run_migrations(&pool)
        .await
        .context("failed to run migrations")?;

    let app = ledger_rs::http::app(pool.clone(), &config);
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.bind_addr))?;

    tracing::info!(bind = %config.bind_addr, "ledger-rs listening");

    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    let result = serve.await.context("server error");

    // Close the pool explicitly rather than letting the process exit tear the
    // sockets down: an in-flight transaction gets a clean ROLLBACK instead of
    // leaving the server to reap an abandoned session, which would hold that
    // account's row lock until `idle_in_transaction_session_timeout` fires.
    tracing::info!("draining the connection pool");
    tokio::time::timeout(config.shutdown_grace, pool.close())
        .await
        .unwrap_or_else(|_| tracing::warn!("pool did not drain within the shutdown grace period"));

    tracing::info!("shutdown complete");
    result
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));

    // Structured JSON when asked for, so a log aggregator gets fields rather
    // than a line it has to regex.
    if std::env::var("LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// Drains in-flight requests on SIGINT/SIGTERM rather than cutting them off
/// mid-transfer. SIGTERM is the one that matters: it is what a container
/// runtime sends before it kills the process.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}
