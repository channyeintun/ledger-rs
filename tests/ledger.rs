//! End-to-end tests against a real Postgres and the real Axum app.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::TestApp;
use futures::future::join_all;
use reqwest::StatusCode;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromStr;
use serde_json::Value;
use uuid::Uuid;

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

async fn error_code(response: reqwest::Response) -> String {
    let body: Value = response.json().await.expect("error body");
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error.code in {body}"))
        .to_string()
}

/// Opens `amount` of new money into `account` from a funding account.
async fn fund(app: &TestApp, funding: Uuid, account: Uuid, amount: &str, currency: &str) {
    app.transfer(
        &format!("fund-{}-{}", account, Uuid::new_v4()),
        funding,
        account,
        amount,
        currency,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_is_created_and_read_back_with_a_zero_balance() {
    let app = TestApp::spawn().await;

    let created = app.create_account("alice", "USD").await;
    assert_eq!(created.name, "alice");
    assert_eq!(created.balance.amount(), Decimal::ZERO);
    assert_eq!(created.balance.currency().as_str(), "USD");
    assert!(!created.allows_negative_balance);

    let fetched = app.get_account(created.id).await;
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.balance.amount(), Decimal::ZERO);

    app.assert_invariants_hold().await;
}

#[tokio::test]
async fn unknown_account_is_404_and_unknown_currency_is_422() {
    let app = TestApp::spawn().await;

    let response = app
        .client
        .get(format!("{}/accounts/{}", app.base_url, Uuid::new_v4()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "account_not_found");

    // A typo must not silently open an account in a currency nobody else uses.
    let response = app.create_account_raw("typo", "USDD", false).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let response = app.create_account_raw("typo", "XYZ", false).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transfer_moves_money_and_records_exactly_two_balancing_entries() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;

    fund(&app, funding.id, alice.id, "100", "USD").await;

    let detail = app.transfer("t-1", alice.id, bob.id, "25.50", "USD").await;

    assert_eq!(detail.entries.len(), 2);
    assert!(detail.is_balanced());

    let debit = detail
        .entries
        .iter()
        .find(|e| e.direction == ledger_rs::domain::Direction::Debit)
        .expect("a debit entry");
    let credit = detail
        .entries
        .iter()
        .find(|e| e.direction == ledger_rs::domain::Direction::Credit)
        .expect("a credit entry");

    // Debit-positive: the receiver is debited, the sender credited.
    assert_eq!(debit.account_id, bob.id);
    assert_eq!(credit.account_id, alice.id);
    assert_eq!(debit.amount.amount(), dec("25.50"));

    assert_eq!(app.balance(alice.id).await, dec("74.50"));
    assert_eq!(app.balance(bob.id).await, dec("25.50"));
    // The funding account holds the negative side; invariant #3 still nets zero.
    assert_eq!(app.balance(funding.id).await, dec("-100"));

    let fetched = app.get_transaction_raw(detail.transaction.id).await;
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched: ledger_rs::domain::TransactionDetail = fetched.json().await.unwrap();
    assert_eq!(fetched.entries.len(), 2);
    assert!(fetched.is_balanced());

    app.assert_invariants_hold().await;
}

#[tokio::test]
async fn transfer_rejects_bad_requests_without_moving_money() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    let euro = app.create_account("euro", "EUR").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    // Missing Idempotency-Key.
    let response = app
        .client
        .post(format!("{}/transfers", app.base_url))
        .json(&serde_json::json!({
            "from_account_id": alice.id,
            "to_account_id": bob.id,
            "amount": "1",
            "currency": "USD",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(response).await, "missing_idempotency_key");

    let cases: Vec<(&str, Uuid, Uuid, &str, &str, StatusCode, &str)> = vec![
        (
            "overdraft",
            alice.id,
            bob.id,
            "100.01",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "insufficient_funds",
        ),
        (
            "self-transfer",
            alice.id,
            alice.id,
            "1",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "self_transfer",
        ),
        (
            "cross-currency",
            alice.id,
            euro.id,
            "1",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "currency_mismatch",
        ),
        (
            "zero amount",
            alice.id,
            bob.id,
            "0",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "non_positive_amount",
        ),
        (
            "negative amount",
            alice.id,
            bob.id,
            "-5",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "non_positive_amount",
        ),
        (
            // Finer than NUMERIC(20,8): rejected at the edge rather than
            // silently rounded on INSERT.
            "sub-precision amount",
            alice.id,
            bob.id,
            "0.000000001",
            "USD",
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_amount",
        ),
        (
            "unknown account",
            alice.id,
            Uuid::new_v4(),
            "1",
            "USD",
            StatusCode::NOT_FOUND,
            "account_not_found",
        ),
    ];

    for (label, from, to, amount, currency, status, code) in cases {
        let response = app
            .transfer_raw(
                &format!("bad-{label}"),
                from,
                to,
                amount,
                currency,
                "should fail",
            )
            .await;
        assert_eq!(response.status(), status, "{label}");
        assert_eq!(error_code(response).await, code, "{label}");
    }

    // Nothing moved, and no rejected attempt left a partial transaction behind.
    assert_eq!(app.balance(alice.id).await, dec("100"));
    assert_eq!(app.balance(bob.id).await, Decimal::ZERO);
    assert_eq!(
        app.transaction_count().await,
        1,
        "only the funding transfer"
    );
    assert_eq!(app.entry_count().await, 2);

    app.assert_invariants_hold().await;
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replay_with_the_same_payload_returns_the_original_result() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    let first = app
        .transfer_raw("replay-key", alice.id, bob.id, "10", "USD", "rent")
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first: ledger_rs::domain::TransactionDetail = first.json().await.unwrap();

    // Same key, same payload — including an amount spelled differently.
    for amount in ["10", "10.00", "10.000"] {
        let replay = app
            .transfer_raw("replay-key", alice.id, bob.id, amount, "USD", "rent")
            .await;
        assert_eq!(replay.status(), StatusCode::OK, "replay with {amount}");
        let replay: ledger_rs::domain::TransactionDetail = replay.json().await.unwrap();
        assert_eq!(replay.transaction.id, first.transaction.id);
        assert_eq!(replay.entries.len(), 2);
    }

    // Money moved exactly once.
    assert_eq!(app.balance(alice.id).await, dec("90"));
    assert_eq!(app.balance(bob.id).await, dec("10"));
    assert_eq!(app.entry_count().await, 4, "funding + one transfer");

    app.assert_invariants_hold().await;
}

#[tokio::test]
async fn same_key_with_a_different_payload_is_a_conflict() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    let carol = app.create_account("carol", "USD").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    app.transfer_raw("conflict-key", alice.id, bob.id, "10", "USD", "rent")
        .await;

    let variants = [
        (alice.id, bob.id, "11", "rent"),      // amount differs
        (alice.id, carol.id, "10", "rent"),    // destination differs
        (alice.id, bob.id, "10", "utilities"), // description differs
    ];

    for (from, to, amount, description) in variants {
        let response = app
            .transfer_raw("conflict-key", from, to, amount, "USD", description)
            .await;
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "{amount}/{description}"
        );
        assert_eq!(error_code(response).await, "idempotency_key_conflict");
    }

    assert_eq!(app.balance(alice.id).await, dec("90"));
    assert_eq!(app.transaction_count().await, 2, "funding + one transfer");

    app.assert_invariants_hold().await;
}

/// A rejected transfer releases its idempotency key, so the caller may retry
/// it — and if the world has changed in between, the retry legitimately moves
/// money. Pinned here because it is a deliberate trade-off rather than a
/// consequence nobody chose; see the module docs on `db::transfers`.
#[tokio::test]
async fn a_rejected_transfer_does_not_burn_its_idempotency_key() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "10", "USD").await;

    // Rejected: alice holds 10.
    let response = app
        .transfer_raw("retry-key", alice.id, bob.id, "50", "USD", "rent")
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(response).await, "insufficient_funds");

    // The rejection left nothing behind — not even the transaction row.
    assert_eq!(
        app.transaction_count().await,
        1,
        "only the funding transfer"
    );

    // Top alice up, then retry the very same key and payload.
    fund(&app, funding.id, alice.id, "100", "USD").await;
    let response = app
        .transfer_raw("retry-key", alice.id, bob.id, "50", "USD", "rent")
        .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a released key must be reusable"
    );

    assert_eq!(app.balance(alice.id).await, dec("60"));
    assert_eq!(app.balance(bob.id).await, dec("50"));

    // And now it is a genuine key: replaying it is a no-op.
    let replay = app
        .transfer_raw("retry-key", alice.id, bob.id, "50", "USD", "rent")
        .await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(app.balance(bob.id).await, dec("50"));

    app.assert_invariants_hold().await;
}

/// Required: the same key sent twice concurrently creates exactly one
/// transaction.
///
/// Run with more than two racers, because the interesting interleaving is the
/// one where a loser blocks on the unique index while the winner is still
/// uncommitted — with only two requests that is easy to miss by luck.
#[tokio::test]
async fn concurrent_requests_with_one_key_create_exactly_one_transaction() {
    let app = Arc::new(TestApp::spawn_concurrent().await);

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    const RACERS: usize = 16;

    let responses = join_all((0..RACERS).map(|_| {
        let app = Arc::clone(&app);
        async move {
            let response = app
                .transfer_raw("race-key", alice.id, bob.id, "10", "USD", "rent")
                .await;
            let status = response.status();
            let detail: ledger_rs::domain::TransactionDetail = response.json().await.expect("body");
            (status, detail)
        }
    }))
    .await;

    let created = responses
        .iter()
        .filter(|(status, _)| *status == StatusCode::CREATED)
        .count();
    let replayed = responses
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .count();

    assert_eq!(created, 1, "exactly one racer may create the transaction");
    assert_eq!(replayed, RACERS - 1, "every other racer must see a replay");

    // All racers describe the same transaction.
    let ids: std::collections::HashSet<_> = responses
        .iter()
        .map(|(_, detail)| detail.transaction.id)
        .collect();
    assert_eq!(ids.len(), 1, "racers disagreed about the transaction id");

    // Money moved exactly once, and no duplicate entries exist.
    assert_eq!(app.balance(alice.id).await, dec("90"));
    assert_eq!(app.balance(bob.id).await, dec("10"));
    assert_eq!(app.transaction_count().await, 2, "funding + one transfer");
    assert_eq!(app.entry_count().await, 4);

    app.assert_entries_are_well_formed().await;
    app.assert_invariants_hold().await;
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Required: 100 parallel transfers draining one account.
///
/// Asserts the final balance is exactly zero, that no negative intermediate
/// state is ever observable, and that no entry was lost or duplicated.
#[tokio::test]
async fn one_hundred_parallel_transfers_drain_an_account_to_exactly_zero() {
    let app = Arc::new(TestApp::spawn_concurrent().await);

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;

    const TRANSFERS: usize = 100;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    // Watch the draining account throughout. The CHECK constraint already makes
    // a negative balance unrepresentable; this proves it independently, and
    // would also catch a balance that dipped negative and was "corrected".
    let stop = Arc::new(AtomicBool::new(false));
    let observer = {
        let app = Arc::clone(&app);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut samples = 0_u64;
            let mut lowest = dec("100");
            while !stop.load(Ordering::Relaxed) {
                let observed: Decimal =
                    sqlx::query_scalar("SELECT balance FROM accounts WHERE id = $1")
                        .bind(alice.id)
                        .fetch_one(&app.pool)
                        .await
                        .expect("sample balance");
                assert!(
                    observed >= Decimal::ZERO,
                    "observed a negative intermediate balance: {observed}"
                );
                lowest = lowest.min(observed);
                samples += 1;
                tokio::task::yield_now().await;
            }
            (samples, lowest)
        })
    };

    let results = join_all((0..TRANSFERS).map(|i| {
        let app = Arc::clone(&app);
        async move {
            app.transfer_raw(&format!("drain-{i}"), alice.id, bob.id, "1", "USD", "drain")
                .await
                .status()
        }
    }))
    .await;

    stop.store(true, Ordering::Relaxed);
    let (samples, lowest) = observer.await.expect("observer task");
    assert!(samples > 0, "the observer never sampled the balance");

    let created = results
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(
        created, TRANSFERS,
        "every transfer was funded and must have succeeded; got {results:?}"
    );

    assert_eq!(
        app.balance(alice.id).await,
        Decimal::ZERO,
        "the drained account must land on exactly zero"
    );
    assert_eq!(app.balance(bob.id).await, dec("100"));
    assert!(lowest >= Decimal::ZERO);

    // No lost or duplicated entries: one transaction per transfer, plus the
    // funding transfer, and two entries each.
    assert_eq!(app.transaction_count().await, TRANSFERS as i64 + 1);
    assert_eq!(app.entry_count().await, (TRANSFERS as i64 + 1) * 2);
    app.assert_entries_are_well_formed().await;

    app.assert_invariants_hold().await;
}

/// Oversubscribing the same account under concurrency: exactly as many
/// transfers succeed as there is money to cover, and not one more.
#[tokio::test]
async fn concurrent_oversubscription_never_overdraws() {
    let app = Arc::new(TestApp::spawn_concurrent().await);

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;

    const ATTEMPTS: usize = 150;
    const FUNDED: usize = 100;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    let results = join_all((0..ATTEMPTS).map(|i| {
        let app = Arc::clone(&app);
        async move {
            app.transfer_raw(
                &format!("oversub-{i}"),
                alice.id,
                bob.id,
                "1",
                "USD",
                "oversubscribe",
            )
            .await
            .status()
        }
    }))
    .await;

    let created = results
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    let rejected = results
        .iter()
        .filter(|s| **s == StatusCode::UNPROCESSABLE_ENTITY)
        .count();

    assert_eq!(created, FUNDED, "exactly the funded amount may go through");
    assert_eq!(rejected, ATTEMPTS - FUNDED, "the rest must be rejected");

    assert_eq!(app.balance(alice.id).await, Decimal::ZERO);
    assert_eq!(app.balance(bob.id).await, dec("100"));

    // A rejected transfer must leave nothing behind — not even its transaction
    // row, so the client can retry the same key.
    assert_eq!(app.transaction_count().await, FUNDED as i64 + 1);
    app.assert_entries_are_well_formed().await;
    app.assert_invariants_hold().await;
}

/// The row lock, tested in isolation.
///
/// With the `CHECK` constraint in place, removing `SELECT ... FOR UPDATE`
/// changes nothing observable: the constraint plus `balance = balance + $1`
/// still refuses every overdraft. That is defense in depth working as intended,
/// but it also means no other test in this file actually exercises the lock —
/// a mutation that deletes it passes the whole suite.
///
/// So drop the constraint first, leaving the application-level check as the
/// only thing standing between 150 concurrent requests and an account holding
/// 100. It survives only if the read of the balance and the debit it authorizes
/// are genuinely one atomic step. Without the lock this overdraws.
#[tokio::test]
async fn the_balance_check_is_linearizable_even_without_the_check_constraint() {
    let app = TestApp::spawn_concurrent().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;

    const ATTEMPTS: usize = 150;
    const FUNDED: usize = 100;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    // Remove the database backstop for this test database only.
    sqlx::query("ALTER TABLE accounts DROP CONSTRAINT accounts_balance_non_negative")
        .execute(&app.pool)
        .await
        .expect("drop the non-negative balance constraint");

    let app = Arc::new(app);
    let results = join_all((0..ATTEMPTS).map(|i| {
        let app = Arc::clone(&app);
        async move {
            app.transfer_raw(
                &format!("unguarded-{i}"),
                alice.id,
                bob.id,
                "1",
                "USD",
                "unguarded",
            )
            .await
            .status()
        }
    }))
    .await;

    let created = results
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();

    assert_eq!(
        created, FUNDED,
        "with no CHECK constraint, only the row lock prevents an overdraft"
    );
    assert_eq!(
        app.balance(alice.id).await,
        Decimal::ZERO,
        "the account overdrew: the balance check and the debit were not atomic"
    );
    assert_eq!(app.balance(bob.id).await, dec("100"));

    // The invariant sweep checks negative balances independently of the
    // constraint we just dropped, so it still means something here.
    app.assert_invariants_hold().await;
}

/// Transfers in opposite directions between the same pair of accounts must not
/// deadlock. Without the UUID-ordered lock acquisition this is the classic
/// deadlock, and Postgres would abort one side with SQLSTATE 40P01.
#[tokio::test]
async fn opposing_concurrent_transfers_do_not_deadlock() {
    let app = Arc::new(TestApp::spawn_concurrent().await);

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "500", "USD").await;
    fund(&app, funding.id, bob.id, "500", "USD").await;

    const ROUNDS: usize = 60;

    let results = join_all((0..ROUNDS).map(|i| {
        let app = Arc::clone(&app);
        async move {
            // Half go A->B, half go B->A, all at once.
            let (from, to) = if i % 2 == 0 {
                (alice.id, bob.id)
            } else {
                (bob.id, alice.id)
            };
            app.transfer_raw(&format!("cross-{i}"), from, to, "1", "USD", "cross")
                .await
                .status()
        }
    }))
    .await;

    for (i, status) in results.iter().enumerate() {
        assert_eq!(
            *status,
            StatusCode::CREATED,
            "transfer {i} failed — a deadlock abort would surface as a 500"
        );
    }

    // Equal numbers each way, so both balances return to where they started.
    assert_eq!(app.balance(alice.id).await, dec("500"));
    assert_eq!(app.balance(bob.id).await, dec("500"));

    app.assert_invariants_hold().await;
}

// ---------------------------------------------------------------------------
// Immutability
// ---------------------------------------------------------------------------

/// The ledger is append-only at the database level, not merely by convention:
/// these statements bypass the application entirely and must still fail.
#[tokio::test]
async fn the_ledger_rejects_updates_and_deletes_even_via_raw_sql() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;
    let detail = app.transfer("imm-1", alice.id, bob.id, "10", "USD").await;

    let forbidden: Vec<(&str, &'static str)> = vec![
        ("update an entry amount", "UPDATE entries SET amount = 1"),
        ("delete an entry", "DELETE FROM entries"),
        (
            "edit a transaction",
            "UPDATE transactions SET description = 'edited'",
        ),
        ("delete a transaction", "DELETE FROM transactions"),
        ("delete an account", "DELETE FROM accounts"),
        ("rename an account", "UPDATE accounts SET name = 'mallory'"),
        (
            "grant an account an overdraft after the fact",
            "UPDATE accounts SET allows_negative_balance = TRUE",
        ),
    ];

    for (label, statement) in forbidden {
        let result = sqlx::query(statement).execute(&app.pool).await;
        assert!(result.is_err(), "{label} should have been rejected");
    }

    // Appending a balanced pair of entries to an already-committed transaction
    // would pass the balance check but silently rewrite what that transaction
    // means. The same-transaction trigger forbids it.
    let appended = sqlx::query(
        "INSERT INTO entries (id, transaction_id, account_id, direction, amount, currency)
         VALUES (gen_random_uuid(), $1, $2, 'debit', 5, 'USD'),
                (gen_random_uuid(), $1, $3, 'credit', 5, 'USD')",
    )
    .bind(detail.transaction.id)
    .bind(bob.id)
    .bind(alice.id)
    .execute(&app.pool)
    .await;
    assert!(
        appended.is_err(),
        "entries must not be appendable to an existing transaction"
    );

    // An overdraft attempted directly against the database is refused too.
    let overdraft = sqlx::query("UPDATE accounts SET balance = balance - 1000 WHERE id = $1")
        .bind(alice.id)
        .execute(&app.pool)
        .await;
    assert!(overdraft.is_err(), "the CHECK constraint must refuse this");

    assert_eq!(app.balance(alice.id).await, dec("90"));
    assert_eq!(app.balance(bob.id).await, dec("10"));
    app.assert_invariants_hold().await;
}

/// A correction is a new reversal transaction, never an edit — and the reversal
/// itself is an ordinary balanced transfer.
#[tokio::test]
async fn corrections_are_reversal_transactions() {
    let app = TestApp::spawn().await;

    let funding = app.create_funding_account("funding", "USD").await;
    let alice = app.create_account("alice", "USD").await;
    let bob = app.create_account("bob", "USD").await;
    fund(&app, funding.id, alice.id, "100", "USD").await;

    let original = app.transfer("orig", alice.id, bob.id, "30", "USD").await;
    assert_eq!(app.balance(alice.id).await, dec("70"));

    // Reverse it by moving the same amount back.
    let reversal = app
        .transfer("reversal", bob.id, alice.id, "30", "USD")
        .await;

    assert_ne!(reversal.transaction.id, original.transaction.id);
    assert_eq!(app.balance(alice.id).await, dec("100"));
    assert_eq!(app.balance(bob.id).await, Decimal::ZERO);

    // Both transactions survive; history is added to, never rewritten.
    assert_eq!(app.transaction_count().await, 3);
    assert_eq!(app.entry_count().await, 6);

    app.assert_invariants_hold().await;
}
