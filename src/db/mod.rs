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

pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
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
