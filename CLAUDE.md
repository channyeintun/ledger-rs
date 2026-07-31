# CLAUDE.md — conventions for ledger-rs

Read `migrations/0001_init.sql` before changing anything. The schema, not the
Rust, is where the ledger's guarantees live.

## Non-negotiables

1. **No `f32`/`f64`/`REAL`/`DOUBLE PRECISION` anywhere near money.** Ever, for
   any reason, including tests, fixtures, and benchmarks. `rust_decimal::Decimal`
   in Rust, `NUMERIC(20, 8)` in Postgres, JSON **strings** on the wire.
2. **No bare `Decimal` in a signature that means "money".** Use
   `domain::Money`, which carries its currency and refuses cross-currency
   arithmetic. A bare `Decimal` is only acceptable inside `db/` where it is
   being bound to or read from a single `NUMERIC` column, and it must be
   recombined into a `Money` before it leaves the module.
3. **`entries` and `transactions` are append-only.** No `UPDATE`, no `DELETE`,
   no exceptions. Database triggers enforce this; do not add a bypass.
   Corrections are new reversal transactions that reference the original.
4. **Money amounts on the wire are JSON strings.** A bare JSON number is parsed
   as an IEEE-754 double by most clients and loses precision before it reaches
   us. `#[serde(with = "rust_decimal::serde::str")]` on every amount field.
5. **No ORM, no query builder.** Raw SQL, written out as literal strings at the
   call site, so that locking behaviour is visible where it happens.

## The four invariants

| # | Invariant | Enforced by |
|---|---|---|
| 1 | Debits − credits nets to exactly zero within a transaction | `entries_balanced_trigger` (deferred to `COMMIT`) + `entries_same_xact_trigger`, which makes a transaction's entry set unextendable afterwards |
| 2 | No account balance goes below zero | `CHECK (allows_negative_balance OR balance >= 0)` + UUID-ordered `SELECT ... FOR UPDATE` in `db::transfers::execute` |
| 3 | The sum of all entries system-wide is zero | Follows from #1; verified by `ledger_check_invariants()` |
| 4 | Idempotent replays never create duplicate entries | `UNIQUE (idempotency_key)`, claimed *before* any balance is touched |

Every one of these has both a database-level control and a test. Adding a
feature that touches money means adding to both.

## Sign convention

`balance = SUM(debits) − SUM(credits)` — **debit-positive**. A transfer of X
from A to B *credits* A (balance falls) and *debits* B (balance rises).
`amount` is always positive; the sign lives in `direction`, never in the amount.

## Why `allows_negative_balance` exists

Invariants #2 and #3 are jointly satisfiable only if some account may hold a
credit-normal (negative) position. Otherwise the only reachable state is
all-balances-zero and money can never enter the system. Funding/equity accounts
set this flag; customer accounts never do, and the field defaults to `false` so
the unsafe case is never what you get by forgetting a field.

## Scope limit on the non-negative CHECK

A hard `CHECK` makes a negative balance *unrepresentable*, which is only safe
because **this ledger is closed**: every balance change originates from a
transfer authorized inside the same database transaction. There is no
settlement estimate, chargeback, or provider correction that can force a
negative position on us after the fact.

If this service ever ingests an external money source, that constraint must be
relaxed to representable-but-monitored — enforce at authorization time, detect
post-factum, book and recover explicitly. Code that cannot represent the state
it is forced into will either abort mid-flow or clamp to zero, and **clamping
mints money**.

## `accounts.balance` is a cache

It is the one mutable number in the system. The entry log is the record; the
column exists so reads are O(1) and so invariant #2 can be a `CHECK`. Anything
that writes it must write the corresponding entries in the same database
transaction. `ledger_check_invariants()` re-derives every balance from the log
and is the control that catches drift — run it in tests and as a production
sweep.

## Concurrency rules

* Lock accounts with `SELECT ... FOR UPDATE` in **ascending UUID order**, always,
  as separate statements. Sorting gives every transaction the same lock
  sequence, so A→B and B→A queue instead of deadlocking. Using one
  `WHERE id = ANY(...)` would make acquisition order depend on the query planner.
* Write balance updates as `balance = balance + $1`, never
  `balance = $precomputed`. Postgres re-reads the freshest committed row after
  the lock is granted and re-applies the expression, which is what makes
  `READ COMMITTED` sufficient and avoids lost updates.
* Claim the idempotency key **before** touching any balance. The unique index
  is the serialization point for concurrent replays; a read-then-insert check
  is the race it exists to prevent.

## Error handling

Every failure is a named variant of `error::LedgerError` with a stable
machine-readable `code()`. Nothing reaches 500 except a genuine bug or an
unavailable database — a rejected transfer is a typed outcome, because
"insufficient funds" and "the database fell over" demand different retry
behaviour from the caller.

## Testing

* Integration tests use `DATABASE_URL` if set, otherwise spin up Postgres via
  testcontainers. Each test gets its own schema-isolated database.
* Any change to a money path needs a property test, not just examples.
  Enumerated happy-path assertions do not find the bugs that matter here.
* Assert `db::check_invariants` returns all-`ok` after any test that writes.

## Known v0.1.0 simplifications

Deliberate, documented, and worth revisiting before this is load-bearing:

* One `created_at` per record, standing in for value / booking / settlement
  time. Fine for a closed ledger with no external settlement; not fine once
  there is one.
* No reversal endpoint yet — the schema supports it (reversals are ordinary
  transactions), but nothing exposes it.
* No pagination on entry reads.
* `ledger_check_invariants()` is not scheduled; it is called from tests only.
