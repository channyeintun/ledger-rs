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
| `ALLOW_FUNDING_ACCOUNT_CREATION` | `false` | Whether `POST /accounts` may open a money-minting funding account |
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

Because a funding account is exempt from the non-negative balance constraint,
it can create value from nothing — so opening one over HTTP is refused with
`403 funding_account_creation_disabled` unless the operator sets
`ALLOW_FUNDING_ACCOUNT_CREATION=true`. Otherwise "may create an account" would
mean "may mint money", which matters a great deal while there is no
authentication layer.

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
  account and move money between any two accounts. This is the single largest
  gap, and it is deliberately left open because the right answer depends on the
  deployment — mTLS between internal services, an API gateway, or per-tenant
  keys are all defensible and they lead to different schemas. Note the blast
  radius is bounded: minting money requires a funding account, and opening one
  over HTTP is off by default, so an unauthenticated caller can move existing
  value around but cannot create it.
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
- **No erasure path for `accounts.name` or `transactions.description`.** Both
  are caller-supplied and land in immutable storage, so a right-to-erasure
  request cannot be honoured today. Crypto-shredding is the fix — see
  [Why the ledger is immutable](#why-the-ledger-is-immutable).
- **No reversal endpoint.** The schema supports reversals — they are ordinary
  transactions — but nothing exposes one.
- **The non-negative `CHECK` assumes a closed ledger.** See
  [CLAUDE.md](CLAUDE.md#scope-limit-on-the-non-negative-check): if this service
  ever ingests an external money source, that constraint has to be relaxed to
  representable-but-monitored, because code that cannot represent a state it is
  forced into will either abort mid-flow or clamp to zero, and clamping mints
  money.

## Design decisions

### Why double-entry

The obvious design is one mutable `balance` per account plus a log of what
happened to it. That works until the two disagree — and when they do, nothing
in the system can tell you which one is right. You are left reconstructing the
truth from application logs, if you kept them.

Double-entry removes the possibility. Every movement has two sides that sum to
zero, so a bug cannot create or destroy value, only misroute it — and
misrouting is *detectable*, because the total is still zero while the account
is wrong. That is the actual property worth having: it turns "have we lost
money?" from an unanswerable question into a query.

The structural payoff is that global integrity follows from a local rule.
Invariant #1 is per-transaction and cheap, so a trigger can enforce it at
`COMMIT`; invariant #3 is a statement about the entire system and is a
*consequence* of #1, not a separate thing to police. Without that, system-wide
consistency would be a nightly job that tells you about yesterday's corruption.

It also forces you to say where money came from. Single-entry lets you write
`balance += 100` with no counterparty; double-entry demands the other side
exist. That is what surfaced `allows_negative_balance` — the model exposed a
question the original spec had not answered, namely that invariants #2 and #3
are jointly satisfiable only if *some* account may hold a negative position.
A model that makes you answer that before you can store anything is doing its
job.

### Why the ledger is immutable

Editing a row destroys the evidence needed to answer the only question that
matters during an incident or an audit: why is this number what it is?

The line is set by reporting, not by purity. Before a number has been shown to
anyone, correcting it in place is merely untidy. Once a balance has been
reported to a user, a regulator, or an auditor, rewriting the history behind it
changes a number someone already acted on — and it does so invisibly. Since a
ledger cannot know at write time which rows will later be reported, the only
safe rule is that none of them are editable.

Recording corrections as reversals keeps both the mistake and its remedy in the
record. A reversal is an ordinary balanced transaction that references the
original, so nothing about the correction path is special-cased — and the
history shows what was believed, when, and what replaced it.

This is enforced by database trigger rather than convention because a
convention is a comment. A trigger still holds for a superuser `psql` session
during an incident, which is precisely when someone is most tempted to "just
fix the row". `entries` and `transactions` reject `UPDATE` and `DELETE`
outright; `accounts` permits only `balance` to change and can never be deleted.

The accepted cost is that there is no erasure path, and it is worth being
precise about what that implicates. Two fields are caller-supplied and land in
storage that can never be edited or deleted: `accounts.name` and
`transactions.description`. In a payments system an account name is very
plausibly a person's name, so this schema should be assumed to hold personal
data whether or not that was the intent.

That is a genuine tension with a right-to-erasure request, and immutability
wins — a ledger that can be rewritten on request is not a ledger. The
resolution is crypto-shredding rather than deletion: store a reference to an
encrypted blob, and erase by destroying the key, which leaves the entry log
intact and every balance still derivable. That is not implemented here, and it
is the piece to build before this stores anything about an identifiable person.
Bounding both fields at least keeps the exposure finite and known.

### Why idempotency is a header, not a body field

The key describes the *delivery attempt*, not the transfer. Two retries of the
same intent share a key; the intent itself has no opinion about it. Putting it
in the body mixes those two things, and the request fingerprint would then have
to exclude one field by hand — exactly the kind of special case that survives
review once and is forgotten during the next schema change.

Keeping it in a header makes both semantics fall out for free. The fingerprint
is a hash of the intent alone, so the same intent under a new key is a genuinely
new transfer, and the same key with a different intent is unambiguously a
conflict (`409`). Neither case needs a rule of its own.

It also generalises: every future mutating endpoint accepts it identically
without touching its schema. And `Idempotency-Key` is what Stripe and the IETF
draft use, so clients, SDKs, and proxies already know the name.

The trade-off is real — a header is easier to drop by accident than a body
field, since proxies strip them and SDK wrappers forget to forward them. The
mitigation is to make it **required**: a missing key is a `400`, never a
silently non-idempotent write. The failure mode of forgetting the key has to be
a refusal, not a double payment.

### Concurrency strategy

The mechanics are documented at the top of [`src/db/transfers.rs`](src/db/transfers.rs).
This is why this shape, over the two obvious alternatives.

**Versus `SERIALIZABLE` plus a retry loop.** Correct, and it needs no locking
discipline at all, which is genuinely attractive. It loses on this workload
specifically: draining one account with 100 concurrent transfers means every
transaction conflicts with every other on the same row. Postgres' SSI would
abort nearly all of them, and the retry loop — not the work — becomes the
throughput. Pessimistic locking on a known-hot row queues instead of aborting,
which gives the same guarantee with predictable latency. A bounded retry on
`40001`/`40P01` is still kept, because this service can lose a deadlock to
something outside it (a migration, a maintenance script) even though transfers
cannot deadlock against each other.

**Versus an event-sourced balance** with no `balance` column, locking the
account row purely as a mutex and recomputing `SUM(entries)` on demand. One
source of truth, nothing to reconcile. Rejected for two reasons: overdraft
prevention degrades to an application-level check with no database backstop —
the database would happily store a negative position — and every balance read
becomes O(entries). The usual fix is periodic snapshots, which is a
materialized balance with extra steps and a staleness window.

Two details carry most of the weight. Locks are acquired in **ascending UUID
order**, as separate statements: sorting gives every transaction in the system
the same lock sequence, so A→B and B→A queue instead of deadlocking, and
separate statements keep acquisition order a property of this code rather than
of whatever row order the query planner happens to pick. And balance updates
are written `balance = balance + $1`, never `balance = $precomputed` — after
the lock is granted Postgres re-reads the freshest committed row and re-applies
the expression, which is what makes `READ COMMITTED` sufficient. The lock and
the expression form are load-bearing together; writing the precomputed form
would reintroduce the lost update the lock exists to prevent.

One honest finding, from mutation-testing the suite. With the `CHECK`
constraint in place, removing `SELECT ... FOR UPDATE` entirely changes nothing
observable — the constraint plus the atomic update still refuses every
overdraft. So the row lock is not, strictly, what enforces invariant #2. What
it does is make the balance check and the debit it authorizes one linearizable
step, so the correctness argument does not depend on the constraint; make lock
ordering deadlock-free; and turn an overdraft into a clean `422` rather than a
transaction abort. That is defense in depth working as intended, but it also
meant nothing in the suite actually exercised the lock. There is now a test
that drops the constraint first so the lock is the only defense left — it
overdraws by ten when the lock is removed.

### Why `accounts.balance` is materialized

Caching a balance is the thing every guide tells you not to do in a ledger.
It earns its place here for two reasons, and only under a specific condition.

First, it makes invariant #2 a `CHECK` constraint. A negative balance becomes
*unrepresentable* rather than merely rejected by code — no future caller, no
migration script, no manual session can produce one. A balance derived from a
`SUM` cannot be expressed as a constraint at all, so the alternative is
application-level enforcement with nothing underneath it. Second, reads are
O(1). An account that has transacted for a year should not cost more to read
than one opened this morning.

The cost is drift, and that is only acceptable because of two things. Every
write to the column happens in the same database transaction as the entries
that justify it, through exactly one code path. And
`ledger_check_invariants()` re-derives every balance from the entry log and
compares — it is the control that makes the cache safe rather than merely
convenient, and it is asserted after every test that writes and after every
property-test case. In production it belongs on a schedule with an alert on any
`ok = false`; that it is not yet scheduled is listed under
[Before this handles real money](#before-this-handles-real-money).

The rule for anything built on top of this: the entry log is the record and the
column is an optimization that must always be provable from it. Code that
writes one without the other is a bug, however convenient it looks.

## License

MIT — see [LICENSE](LICENSE).
