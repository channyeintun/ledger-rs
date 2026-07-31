//! Property tests: random sequences of valid transfers must never violate
//! invariants 1–3.
//!
//! Enumerated happy-path assertions do not find the bugs that matter in a
//! ledger. These generate arbitrary sequences instead and assert the invariants
//! afterwards — both through `ledger_check_invariants()` in SQL and
//! independently in Rust, so a bug in the sweep itself cannot hide a bug in the
//! ledger.
//!
//! Every case runs against the *same* ledger, each with its own fresh set of
//! accounts. That is deliberate: the invariants are statements about the whole
//! system, so checking them against a ledger that already carries the history
//! of every earlier case is strictly stronger than checking an empty one — and
//! it avoids provisioning a database per case.

mod common;

use std::sync::OnceLock;

use common::TestApp;
use ledger_rs::db;
use ledger_rs::domain::{Currency, Money, TransferIntent};
use ledger_rs::error::LedgerError;
use proptest::prelude::*;
use rust_decimal::Decimal;
use tokio::sync::OnceCell;
use uuid::Uuid;

const ACCOUNTS: usize = 4;
const CURRENCY: &str = "USD";

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build a tokio runtime")
    })
}

async fn app() -> &'static TestApp {
    static APP: OnceCell<TestApp> = OnceCell::const_new();
    APP.get_or_init(TestApp::spawn).await
}

fn usd() -> Currency {
    Currency::try_from(CURRENCY.to_string()).unwrap()
}

/// Amounts are generated in minor units and scaled, so the generator can never
/// produce a value finer than the ledger stores.
fn money(minor_units: i64) -> Money {
    Money::new(Decimal::new(minor_units, 2), usd()).expect("representable amount")
}

#[derive(Debug, Clone, Copy)]
struct Op {
    from: usize,
    to: usize,
    minor_units: i64,
}

fn op() -> impl Strategy<Value = Op> {
    (0..ACCOUNTS, 0..ACCOUNTS, 1_i64..20_000).prop_map(|(from, to, minor_units)| Op {
        from,
        to,
        minor_units,
    })
}

/// One transfer that actually landed: its op index and the transaction it made.
#[derive(Debug, Clone, Copy)]
struct Applied {
    op_index: usize,
    transaction_id: Uuid,
}

/// Applies a sequence of transfers, tolerating the rejections that are correct
/// behaviour and failing on anything else. Returns only the ones that landed.
async fn apply(
    app: &TestApp,
    accounts: &[Uuid],
    ops: &[Op],
    key_prefix: &str,
) -> Result<Vec<Applied>, TestCaseError> {
    let mut applied = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        let intent = TransferIntent {
            from_account_id: accounts[op.from],
            to_account_id: accounts[op.to],
            amount: money(op.minor_units),
            description: format!("op {i}"),
        };

        match db::transfers::execute(&app.pool, &format!("{key_prefix}-{i}"), &intent).await {
            Ok(result) => applied.push(Applied {
                op_index: i,
                transaction_id: result.detail.transaction.id,
            }),
            // Correct rejections: the generator is free to produce them.
            Err(LedgerError::InsufficientFunds { .. }) | Err(LedgerError::SelfTransfer) => {}
            Err(other) => {
                return Err(TestCaseError::fail(format!(
                    "unexpected error on op {i} ({op:?}): {other}"
                )));
            }
        }
    }

    Ok(applied)
}

/// Re-derives the invariants in Rust rather than trusting the SQL sweep to
/// check itself. Global statements are checked over the whole ledger; the
/// per-account ones over this case's accounts.
async fn assert_invariants_independently(
    app: &TestApp,
    accounts: &[Uuid],
) -> Result<(), TestCaseError> {
    // Invariant #3, from the entry log.
    let global_net: Option<Decimal> = sqlx::query_scalar(
        "SELECT SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END) FROM entries",
    )
    .fetch_one(&app.pool)
    .await
    .expect("global entry sum");
    prop_assert_eq!(
        global_net.unwrap_or(Decimal::ZERO),
        Decimal::ZERO,
        "invariant #3: entries across the system must net to zero"
    );

    // Invariant #3 again, from the materialized balances. Both must hold, and
    // they are computed from different columns.
    let balance_total: Option<Decimal> = sqlx::query_scalar("SELECT SUM(balance) FROM accounts")
        .fetch_one(&app.pool)
        .await
        .expect("global balance sum");
    prop_assert_eq!(
        balance_total.unwrap_or(Decimal::ZERO),
        Decimal::ZERO,
        "invariant #3: account balances must sum to zero"
    );

    for id in accounts {
        let account = db::accounts::get(&app.pool, *id)
            .await
            .expect("read account");
        let balance = account.balance.amount();

        // Invariant #2: these are all customer accounts.
        prop_assert!(
            balance >= Decimal::ZERO,
            "invariant #2: account {} went negative ({})",
            id,
            balance
        );

        // The materialized cache still agrees with the entry log.
        let derived: Option<Decimal> = sqlx::query_scalar(
            "SELECT SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END)
             FROM entries WHERE account_id = $1",
        )
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .expect("derived balance");
        prop_assert_eq!(
            balance,
            derived.unwrap_or(Decimal::ZERO),
            "materialized balance drifted from the entry log for account {}",
            id
        );
    }

    // Invariant #1, transaction by transaction, through the public read path.
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM transactions")
        .fetch_all(&app.pool)
        .await
        .expect("transaction ids");
    for id in ids {
        let detail = db::transactions::get(&app.pool, id)
            .await
            .expect("read transaction");
        prop_assert!(
            detail.is_balanced(),
            "invariant #1: transaction {} does not balance",
            id
        );
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        ..ProptestConfig::default()
    })]

    /// Random sequences of transfers never violate invariants 1–3, and
    /// replaying the whole sequence creates nothing new (invariant #4).
    #[test]
    fn random_transfer_sequences_preserve_every_invariant(
        openings in prop::collection::vec(0_i64..50_000, ACCOUNTS),
        ops in prop::collection::vec(op(), 1..24),
    ) {
        runtime().block_on(async {
            let app = app().await;
            let case = Uuid::new_v4().simple().to_string();

            let transactions_at_start = app.transaction_count().await;
            let entries_at_start = app.entry_count().await;

            let funding = app
                .create_funding_account(&format!("funding-{case}"), CURRENCY)
                .await;

            let mut accounts = Vec::with_capacity(ACCOUNTS);
            for i in 0..ACCOUNTS {
                accounts.push(
                    app.create_account(&format!("account-{case}-{i}"), CURRENCY)
                        .await
                        .id,
                );
            }

            // Open each account with random funds from the funding account,
            // which is the only account permitted to hold the negative side.
            let mut openings_applied = 0;
            for (i, opening) in openings.iter().enumerate() {
                if *opening == 0 {
                    continue;
                }
                let intent = TransferIntent {
                    from_account_id: funding.id,
                    to_account_id: accounts[i],
                    amount: money(*opening),
                    description: format!("opening balance {i}"),
                };
                db::transfers::execute(&app.pool, &format!("open-{case}-{i}"), &intent)
                    .await
                    .expect("opening transfer");
                openings_applied += 1;
            }

            let key_prefix = format!("op-{case}");
            let applied = apply(app, &accounts, &ops, &key_prefix).await?;

            app.assert_invariants_hold().await;
            assert_invariants_independently(app, &accounts).await?;

            // Every successful transfer contributed exactly two entries, and
            // nothing else did.
            let transactions_now = app.transaction_count().await;
            let entries_now = app.entry_count().await;
            prop_assert_eq!(
                entries_now - entries_at_start,
                ((applied.len() + openings_applied) * 2) as i64,
                "entry count does not match the number of successful transfers"
            );

            // Invariant #4: replaying a transfer that *landed* must return the
            // original transaction and create nothing.
            //
            // Only the ones that landed. A transfer rejected for insufficient
            // funds does not burn its key — see `db::transfers::execute` — so
            // replaying a previously-rejected op is a genuinely new attempt and
            // may legitimately succeed once later ops have funded the account.
            for entry in &applied {
                let op = ops[entry.op_index];
                let intent = TransferIntent {
                    from_account_id: accounts[op.from],
                    to_account_id: accounts[op.to],
                    amount: money(op.minor_units),
                    description: format!("op {}", entry.op_index),
                };
                let result = db::transfers::execute(
                    &app.pool,
                    &format!("{key_prefix}-{}", entry.op_index),
                    &intent,
                )
                .await
                .expect("replaying a landed transfer must succeed");

                prop_assert_eq!(
                    result.outcome,
                    ledger_rs::domain::TransferOutcome::Replayed,
                    "op {} was re-executed rather than replayed",
                    entry.op_index
                );
                prop_assert_eq!(
                    result.detail.transaction.id,
                    entry.transaction_id,
                    "replay of op {} returned a different transaction",
                    entry.op_index
                );
            }

            prop_assert_eq!(
                app.transaction_count().await, transactions_now,
                "a replay created a new transaction"
            );
            prop_assert_eq!(
                app.entry_count().await, entries_now,
                "a replay created new entries"
            );

            app.assert_entries_are_well_formed().await;
            prop_assert!(transactions_at_start <= transactions_now);

            Ok(())
        })?;
    }
}
