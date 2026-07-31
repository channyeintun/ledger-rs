//! ledger-rs — a double-entry ledger service.
//!
//! Layering, outermost first:
//!
//! * [`http`] — JSON in, JSON out. No business rules.
//! * [`db`] — raw SQL, transactions, and locking. The concurrency argument for
//!   invariant #2 lives in [`db::transfers`].
//! * [`domain`] — the shape of the ledger: [`domain::Money`], accounts,
//!   transactions, entries. No I/O.
//! * [`error`] — the failure taxonomy and its single mapping onto HTTP.
//!
//! Underneath all of it, `migrations/0001_init.sql` carries the constraints
//! that make the invariants unwritable rather than merely checked. Read that
//! file first.
//!
//! Floating point is used nowhere for money: `rust_decimal::Decimal` in Rust,
//! `NUMERIC(20, 8)` in Postgres, JSON strings on the wire.

pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod http;

pub use error::{LedgerError, Result};
