# ledger-rs

A double-entry ledger service in Rust. Postgres for storage, Axum for HTTP,
`rust_decimal` for every monetary amount, and no floating point anywhere.

The interesting part is `migrations/0001_init.sql`: the ledger's invariants are
database constraints, not application conventions.

## Status

v0.1.0 — accounts, transfers, and the invariant machinery.

## Quickstart

**Postgres 13 or later is required** — the schema uses `xid8` and
`pg_current_xact_id()` to guarantee that a transaction's entries are written in
the same database transaction that created it. CI and the test harness pin 17.

```bash
docker run -d --name ledger-pg -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=ledger -p 5432:5432 postgres:17
```

```bash
DATABASE_URL=postgres://postgres:postgres@localhost:5432/ledger cargo run
```

Migrations run automatically at startup, under a Postgres advisory lock, so
running every replica of a rolling deploy is safe.

See [`.env.example`](.env.example) for the full set of variables. The ones worth
knowing:

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | *(required)* | Postgres connection string |
| `BIND_ADDR` | `0.0.0.0:3000` | Listen address |
| `DATABASE_MAX_CONNECTIONS` | `16` | Pool size |
| `DATABASE_LOCK_TIMEOUT_SECS` | `5` | Row-lock wait ceiling |
| `DATABASE_STATEMENT_TIMEOUT_SECS` | `30` | Statement ceiling; must exceed the lock timeout |
| `DATABASE_IDLE_IN_TXN_TIMEOUT_SECS` | `30` | Kills sessions holding locks with no progress |
| `REQUEST_TIMEOUT_SECS` | `30` | Must exceed `DATABASE_ACQUIRE_TIMEOUT_SECS` |
| `RUST_LOG` | `info,sqlx=warn` | Tracing filter |
| `LOG_FORMAT` | `text` | `json` for structured logs |

Startup rejects incoherent combinations rather than discovering them under
load — a request timeout below the connection-acquire timeout, or a statement
timeout at or below the lock timeout, both fail fast with an explanation.

## Running it

```bash
docker build -t ledger-rs .
```

The image is unprivileged (uid 10001) and carries no shell utilities, so point
your orchestrator's probes at the endpoints rather than at a container-level
healthcheck:

| Probe | Endpoint | Meaning |
|---|---|---|
| liveness | `GET /health/live` | The process is running. Touches nothing else — a database outage must not trigger a restart loop. |
| readiness | `GET /health/ready` | The process can serve traffic. Checks the pool; returns `503` when it cannot. |

`SIGTERM` drains in-flight requests and then closes the pool, so a transfer in
flight gets a clean `ROLLBACK` rather than leaving an abandoned session holding
two account row locks.

Every response carries `x-request-id`, propagated from the caller when supplied.

## API

All monetary amounts are JSON **strings**, never numbers — a bare JSON number
is parsed as a double by most clients and loses precision in transit.

### `POST /accounts` → `201`

```json
{ "name": "alice", "currency": "USD", "allows_negative_balance": false }
```

`allows_negative_balance` marks a funding/equity account and defaults to
`false`. See [Why funding accounts exist](#why-funding-accounts-exist).

### `GET /accounts/{id}` → `200`

```json
{
  "id": "...",
  "name": "alice",
  "balance": { "amount": "100.00000000", "currency": "USD" },
  "allows_negative_balance": false,
  "created_at": "..."
}
```

### `POST /transfers` → `201` created, `200` replayed

Requires an `Idempotency-Key` header.

```json
{
  "from_account_id": "...",
  "to_account_id": "...",
  "amount": "25.50",
  "currency": "USD",
  "description": "rent"
}
```

| Situation | Response |
|---|---|
| New key | `201` with the created transaction |
| Same key, same payload | `200` with the original transaction |
| Same key, different payload | `409 idempotency_key_conflict` |
| Sender lacks funds | `422 insufficient_funds` |
| Account currency ≠ transfer currency | `422 currency_mismatch` |
| Missing `Idempotency-Key` | `400 missing_idempotency_key` |

A **rejected** transfer releases its key, so the same key may be retried. That
means a request rejected for insufficient funds and retried after a top-up will
move money. Mint a new key per attempt if you need "this attempt failed,
permanently" semantics.

### `GET /transactions/{id}` → `200`

Returns the transaction with all of its entries.

### Errors

```json
{ "error": { "code": "insufficient_funds", "message": "..." } }
```

`code` is stable and machine-readable; `message` is not — branch on `code`.

## Invariants

| # | Invariant | Enforced by |
|---|---|---|
| 1 | Debits − credits nets to exactly zero within a transaction | Deferred constraint trigger at `COMMIT`, plus a trigger that makes a transaction's entry set unextendable afterwards |
| 2 | No account balance goes below zero | `CHECK` constraint plus UUID-ordered row locks |
| 3 | The sum of all entries system-wide is zero | Follows from #1; verified by `ledger_check_invariants()` |
| 4 | Idempotent replays never create duplicate entries | `UNIQUE (idempotency_key)`, claimed before any balance moves |

`entries` and `transactions` are append-only, enforced by trigger. Corrections
are reversal transactions, never edits.

### Reconciliation

`ledger_check_invariants()` re-derives every balance from the entry log and
returns a row per invariant. It is the post-factum control that catches drift
introduced by anything that bypassed the application:

```sql
SELECT * FROM ledger_check_invariants();
```

Intended to run as a scheduled production sweep; currently exercised by the
test suite.

### Why funding accounts exist

Invariants #2 and #3 are jointly satisfiable only if some account may hold a
negative position — otherwise the only reachable state is all-balances-zero and
money can never enter the system. Accounts created with
`allows_negative_balance: true` hold that credit-normal side.

## Testing

```bash
cargo test
```

Integration tests use `DATABASE_URL` if it is set, and otherwise start Postgres
via testcontainers (Docker required). Each test gets its own isolated database.

```bash
DATABASE_URL=postgres://postgres:postgres@localhost:5432/ledger cargo test
```

The suite covers, beyond the usual:

- 100 parallel transfers draining one account to exactly zero, with no negative
  intermediate state and no lost or duplicated entries
- the same idempotency key sent concurrently, creating exactly one transaction
- property tests over random valid transfer sequences, asserting invariants 1–3
  after every case

## Before this handles real money

The invariants and the concurrency story are production-grade. These are not,
and each is a decision rather than an oversight:

- **There is no authentication or authorization.** Any caller can open an
  account, mark it as a funding account, and move money between any two
  accounts. This is the single largest gap, and it is deliberately left open
  because the right answer depends on the deployment — mTLS between internal
  services, an API gateway, or per-tenant keys are all defensible and they lead
  to different schemas.
- **Idempotency keys are globally unique**, not scoped to a caller. Two clients
  that both use `"1"` will collide, and the second gets someone else's
  transaction back. Scoping the unique index to `(caller_id, idempotency_key)`
  is the fix, and it needs the auth decision above first.
- **No rate limiting.** A single client can saturate the pool.
- **No metrics.** Logs are structured and requests are traced, but there is no
  `/metrics` endpoint, so there is nothing to alert on.
- **`ledger_check_invariants()` is not scheduled.** It is exercised by the test
  suite; in production it should run as a periodic sweep with an alert on any
  `ok = false`. That sweep is the control that catches drift the constraints
  cannot.
- **One `created_at` per record**, standing in for value, booking, and
  settlement time. Fine while the ledger is closed; not fine once anything
  external settles into it.
- **No reversal endpoint.** The schema supports reversals — they are ordinary
  transactions — but nothing exposes one.
- **The non-negative `CHECK` assumes a closed ledger.** See
  [CLAUDE.md](CLAUDE.md#scope-limit-on-the-non-negative-check): if this service
  ever ingests an external money source, that constraint has to be relaxed to
  representable-but-monitored, because code that cannot represent a state it is
  forced into will either abort mid-flow or clamp to zero, and clamping mints
  money.

## Design decisions

<!--
  Prose intentionally left to the author. Headings only — fill these in.
-->

### Why double-entry

> TODO

### Why the ledger is immutable

> TODO

### Why idempotency is a header, not a body field

> TODO

### Concurrency strategy

> TODO — the mechanics are documented in `src/db/transfers.rs`; this section is
> for the reasoning about why this trade-off over SERIALIZABLE or an
> event-sourced balance.

### Why `accounts.balance` is materialized

> TODO

## License

MIT — see [LICENSE](LICENSE).
