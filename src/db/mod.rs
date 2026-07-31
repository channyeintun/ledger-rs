//! Persistence. Raw SQL only — no ORM, no query builder.
//!
//! Every statement in this module is written out in full so that the locking
//! behaviour is visible at the call site. In a ledger, *when* a row lock is
//! taken is part of the correctness argument, and an abstraction that hides it
//! is an abstraction that hides the bug.

pub mod accounts;
pub mod transactions;
pub mod transfers;

use std::time::Duration;

use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Migrations are embedded at compile time, so `cargo build`, `cargo clippy`
/// and CI need no database — only the integration tests do.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Pool and per-connection server-side limits.
///
/// The three server-side timeouts matter more here than in an ordinary service:
/// a transfer holds row locks on two accounts, so a single wedged session
/// blocks every other transfer touching those accounts until someone notices.
/// These bound that blast radius without operator intervention.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    /// Recycles connections so a long-lived pool cannot pin server-side state
    /// (plan caches, a failed-over backend) indefinitely.
    pub max_lifetime: Duration,
    pub idle_timeout: Duration,
    /// Server-side ceiling on any single statement. Counts lock wait time, so
    /// it must exceed `lock_timeout`.
    pub statement_timeout: Duration,
    /// Server-side ceiling on waiting for a row lock. Turns a lock convoy into
    /// a fast, typed failure instead of a pile of stuck requests.
    pub lock_timeout: Duration,
    /// Kills sessions that opened a transaction and stopped making progress —
    /// the specific failure that would otherwise hold an account's row lock
    /// forever after a client vanishes mid-transfer.
    pub idle_in_transaction_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            max_connections: 16,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            max_lifetime: Duration::from_secs(30 * 60),
            idle_timeout: Duration::from_secs(10 * 60),
            statement_timeout: Duration::from_secs(30),
            lock_timeout: Duration::from_secs(5),
            idle_in_transaction_timeout: Duration::from_secs(30),
        }
    }
}

pub async fn connect(database_url: &str, config: PoolConfig) -> Result<PgPool, sqlx::Error> {
    let statement_timeout_ms = config.statement_timeout.as_millis();
    let lock_timeout_ms = config.lock_timeout.as_millis();
    let idle_in_transaction_ms = config.idle_in_transaction_timeout.as_millis();

    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout)
        .max_lifetime(config.max_lifetime)
        .idle_timeout(config.idle_timeout)
        // Applied per connection, so every session carries the limits even if
        // the pool grows later or a connection is replaced after a failover.
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                // Not a dynamic-SQL risk: every interpolated value is an
                // integer derived from typed configuration.
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "SET statement_timeout = {statement_timeout_ms}; \
                     SET lock_timeout = {lock_timeout_ms}; \
                     SET idle_in_transaction_session_timeout = {idle_in_transaction_ms};"
                )))
                .execute(conn)
                .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
}

pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// One row of the invariant sweep defined in `migrations/0001_init.sql`.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct InvariantCheck {
    pub invariant: String,
    pub ok: bool,
    pub detail: String,
}

/// Runs the full invariant suite against live data.
///
/// This is the post-factum rung of the control ladder: the constraints and
/// triggers make violations unwritable, but a schema migration, a manual
/// intervention, or a restore from an inconsistent backup can still introduce
/// drift. Intended to run as a periodic production sweep, and asserted after
/// every property-test case.
pub async fn check_invariants(pool: &PgPool) -> Result<Vec<InvariantCheck>, sqlx::Error> {
    sqlx::query_as::<_, InvariantCheck>(
        "SELECT invariant, ok, detail FROM ledger_check_invariants()",
    )
    .fetch_all(pool)
    .await
}
