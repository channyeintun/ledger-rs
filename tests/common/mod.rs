//! Integration-test harness.
//!
//! Uses `DATABASE_URL` when it is set (CI provides a service container), and
//! otherwise starts Postgres via testcontainers. Either way, every test gets
//! its own freshly migrated database, so tests that count rows or assert
//! global invariants cannot interfere with each other — which matters here,
//! because invariant #3 is a statement about the *whole* system.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use ledger_rs::db;
use ledger_rs::domain::{Account, TransactionDetail};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Connections per test for ordinary tests. Deliberately small: the whole
/// suite runs in parallel against one server, and Postgres defaults to
/// `max_connections = 100`. Exhausting it surfaces as `PoolTimedOut`, which
/// then shows up as a spurious 500 and looks like a ledger bug.
pub const DEFAULT_POOL_SIZE: u32 = 4;

/// Connections for the concurrency tests, which need genuine simultaneity to
/// be worth anything. Twelve writers contending for one account's row lock is
/// ample contention to expose a lost update or a deadlock.
pub const CONCURRENT_POOL_SIZE: u32 = 12;

/// Waiting for a connection from the pool. Far longer than production's 10s on
/// purpose: under a full parallel suite the server is heavily loaded, and a
/// request that merely *queued* too long would surface as a 500 and read as a
/// ledger bug. With this generous, any 500 in a test is a genuine defect.
const TEST_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);

/// One Postgres instance per test binary; one database per test.
static POSTGRES: OnceCell<Arc<PostgresBackend>> = OnceCell::const_new();

struct PostgresBackend {
    admin_url: String,
    /// A database with migrations already applied. Every test database is
    /// cloned from it with `CREATE DATABASE ... TEMPLATE`, which is a file copy
    /// inside Postgres rather than a re-run of every DDL statement in the
    /// migration. With ~20 tests each provisioning a database, running the
    /// migrations every time dominated the suite and starved the concurrency
    /// tests badly enough to make them flaky.
    template: String,
    /// Held so the container outlives every test; `None` when `DATABASE_URL`
    /// pointed at a server we did not start.
    _container: Option<
        Box<
            testcontainers_modules::testcontainers::ContainerAsync<
                testcontainers_modules::postgres::Postgres,
            >,
        >,
    >,
}

async fn backend() -> Arc<PostgresBackend> {
    POSTGRES
        .get_or_init(|| async {
            let (admin_url, container) = match std::env::var("DATABASE_URL") {
                Ok(admin_url) => (admin_url, None),
                Err(_) => {
                    use testcontainers_modules::postgres::Postgres;
                    use testcontainers_modules::testcontainers::ImageExt;
                    use testcontainers_modules::testcontainers::runners::AsyncRunner;

                    // Pinned to match CI. The default tag for this module is
                    // older than Postgres 13, which lacks `xid8` /
                    // `pg_current_xact_id()` — the schema needs both, and the
                    // failure is a confusing migration error that never
                    // mentions the version.
                    //
                    // `max_connections` gets headroom over the default 100
                    // because the whole suite runs in parallel against this one
                    // server.
                    let container = Postgres::default()
                        .with_tag("17-alpine")
                        .with_cmd(["postgres", "-c", "max_connections=300"])
                        .start()
                        .await
                        .expect("failed to start a Postgres container; set DATABASE_URL to use an existing server");

                    let host = container.get_host().await.expect("container host");
                    let port = container
                        .get_host_port_ipv4(5432)
                        .await
                        .expect("container port");

                    (
                        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
                        Some(Box::new(container)),
                    )
                }
            };

            let template = format!("ledger_template_{}", Uuid::new_v4().simple());
            create_database(&admin_url, &template, None).await;

            // Migrate the template once; every test database inherits it.
            let template_url = with_database(&admin_url, &template);
            let pool = db::connect(&template_url, db::PoolConfig::default())
                .await
                .expect("connect to the template database");
            db::run_migrations(&pool)
                .await
                .expect("migrate the template database");
            // `CREATE DATABASE ... TEMPLATE` refuses to run while anything is
            // connected to the template, so this close is required, not tidiness.
            pool.close().await;

            Arc::new(PostgresBackend {
                admin_url,
                template,
                _container: container,
            })
        })
        .await
        .clone()
}

/// Creates a database, optionally cloning an already-migrated template.
async fn create_database(admin_url: &str, name: &str, template: Option<&str>) {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(TEST_ACQUIRE_TIMEOUT)
        .connect(admin_url)
        .await
        .expect("connect to the Postgres admin database");

    // Not a dynamic-SQL risk: both identifiers are UUID-derived names this
    // harness generated, with no external input in them.
    let sql = match template {
        Some(template) => format!(r#"CREATE DATABASE "{name}" TEMPLATE "{template}""#),
        None => format!(r#"CREATE DATABASE "{name}""#),
    };
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(&admin)
        .await
        .expect("create database");

    admin.close().await;
}

/// Replaces the database name in a Postgres connection URL.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let (prefix, _old_db) = base.rsplit_once('/').expect("connection URL has a path");
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// A running ledger: an isolated database, migrations applied, and the real
/// Axum app bound to an ephemeral port.
pub struct TestApp {
    pub pool: PgPool,
    pub base_url: String,
    pub client: reqwest::Client,
}

impl TestApp {
    /// A ledger with a small pool, for tests that are not about concurrency.
    pub async fn spawn() -> TestApp {
        TestApp::spawn_with_pool_size(DEFAULT_POOL_SIZE).await
    }

    /// A ledger sized for parallel load.
    pub async fn spawn_concurrent() -> TestApp {
        TestApp::spawn_with_pool_size(CONCURRENT_POOL_SIZE).await
    }

    pub async fn spawn_with_pool_size(pool_size: u32) -> TestApp {
        let backend = backend().await;

        let database = format!("ledger_test_{}", Uuid::new_v4().simple());
        create_database(&backend.admin_url, &database, Some(&backend.template)).await;

        let url = with_database(&backend.admin_url, &database);
        let pool = db::connect(
            &url,
            db::PoolConfig {
                max_connections: pool_size,
                min_connections: 0,
                acquire_timeout: TEST_ACQUIRE_TIMEOUT,
                ..db::PoolConfig::default()
            },
        )
        .await
        .expect("connect to test database");

        // Cloned from the migrated template, so this validates the recorded
        // migrations rather than applying any.
        db::run_migrations(&pool).await.expect("verify migrations");

        // The full production middleware stack, not just the routes: the
        // timeout, panic-catching, request-id and body-limit layers are part of
        // what these tests are asserting about.
        let config = ledger_rs::config::Config {
            database_url: url.clone(),
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            pool: ledger_rs::db::PoolConfig::default(),
            request_timeout: TEST_ACQUIRE_TIMEOUT + Duration::from_secs(30),
            max_body_bytes: 64 * 1024,
            shutdown_grace: std::time::Duration::from_secs(5),
        };
        let app = ledger_rs::http::app(pool.clone(), &config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        TestApp {
            pool,
            base_url: format!("http://{addr}"),
            client: reqwest::Client::new(),
        }
    }

    // --- request helpers -------------------------------------------------

    pub async fn create_account_raw(
        &self,
        name: &str,
        currency: &str,
        allows_negative_balance: bool,
    ) -> reqwest::Response {
        self.client
            .post(format!("{}/accounts", self.base_url))
            .json(&json!({
                "name": name,
                "currency": currency,
                "allows_negative_balance": allows_negative_balance,
            }))
            .send()
            .await
            .expect("POST /accounts")
    }

    pub async fn create_account(&self, name: &str, currency: &str) -> Account {
        let response = self.create_account_raw(name, currency, false).await;
        assert_eq!(response.status(), StatusCode::CREATED, "creating {name}");
        response.json().await.expect("account body")
    }

    /// A funding account: the credit-normal side that lets money exist without
    /// violating invariant #3.
    pub async fn create_funding_account(&self, name: &str, currency: &str) -> Account {
        let response = self.create_account_raw(name, currency, true).await;
        assert_eq!(response.status(), StatusCode::CREATED, "creating {name}");
        response.json().await.expect("account body")
    }

    pub async fn transfer_raw(
        &self,
        idempotency_key: &str,
        from: Uuid,
        to: Uuid,
        amount: &str,
        currency: &str,
        description: &str,
    ) -> reqwest::Response {
        self.client
            .post(format!("{}/transfers", self.base_url))
            .header("Idempotency-Key", idempotency_key)
            .json(&json!({
                "from_account_id": from,
                "to_account_id": to,
                "amount": amount,
                "currency": currency,
                "description": description,
            }))
            .send()
            .await
            .expect("POST /transfers")
    }

    pub async fn transfer(
        &self,
        idempotency_key: &str,
        from: Uuid,
        to: Uuid,
        amount: &str,
        currency: &str,
    ) -> TransactionDetail {
        let response = self
            .transfer_raw(idempotency_key, from, to, amount, currency, "test transfer")
            .await;
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "transfer {amount} {currency} should have been created: {}",
            response.text().await.unwrap_or_default()
        );
        response.json().await.expect("transaction body")
    }

    pub async fn get_account(&self, id: Uuid) -> Account {
        let response = self
            .client
            .get(format!("{}/accounts/{id}", self.base_url))
            .send()
            .await
            .expect("GET /accounts/{id}");
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("account body")
    }

    pub async fn balance(&self, id: Uuid) -> Decimal {
        self.get_account(id).await.balance.amount()
    }

    pub async fn get_transaction_raw(&self, id: Uuid) -> reqwest::Response {
        self.client
            .get(format!("{}/transactions/{id}", self.base_url))
            .send()
            .await
            .expect("GET /transactions/{id}")
    }

    // --- assertions ------------------------------------------------------

    /// Runs the full invariant sweep and fails with the offending rows.
    ///
    /// Called at the end of every test that writes. The database constraints
    /// make violations unwritable; this is the independent check that they
    /// really did.
    pub async fn assert_invariants_hold(&self) {
        let checks = db::check_invariants(&self.pool)
            .await
            .expect("run ledger_check_invariants()");

        assert!(
            !checks.is_empty(),
            "the invariant sweep returned no rows at all"
        );

        let failures: Vec<_> = checks.iter().filter(|c| !c.ok).collect();
        assert!(
            failures.is_empty(),
            "ledger invariants violated: {failures:#?}"
        );
    }

    pub async fn count(&self, query: &'static str) -> i64 {
        sqlx::query_scalar::<_, i64>(query)
            .fetch_one(&self.pool)
            .await
            .expect("count query")
    }

    pub async fn transaction_count(&self) -> i64 {
        self.count("SELECT count(*) FROM transactions").await
    }

    pub async fn entry_count(&self) -> i64 {
        self.count("SELECT count(*) FROM entries").await
    }

    /// Every transaction has exactly two entries, and no entry id repeats.
    /// This is the "no lost or duplicated entries" check.
    pub async fn assert_entries_are_well_formed(&self) {
        let odd_shape: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (
                 SELECT transaction_id FROM entries
                 GROUP BY transaction_id HAVING count(*) <> 2
             ) t",
        )
        .fetch_one(&self.pool)
        .await
        .expect("entry shape query");
        assert_eq!(odd_shape, 0, "transactions without exactly two entries");

        let distinct: i64 = sqlx::query_scalar("SELECT count(DISTINCT id) FROM entries")
            .fetch_one(&self.pool)
            .await
            .expect("distinct entry ids");
        assert_eq!(
            distinct,
            self.entry_count().await,
            "duplicate entry ids present"
        );
    }
}
