-- ledger-rs v0.1.0 — initial schema.
--
-- Design stance: the ledger of record is (transactions, entries). Both are
-- strictly append-only. `accounts.balance` is a *materialized cache* derived
-- from entries; it is the one mutable number in the system and it exists only
-- so that invariant #2 can be enforced by the database rather than by
-- application code. `ledger_check_invariants()` at the bottom of this file is
-- the post-factum control that proves the cache never drifts from the log.
--
-- Sign convention: balance = SUM(debits) - SUM(credits) ("debit-positive").
-- A transfer of X from A to B credits A (-X) and debits B (+X).

-- Postgres 13+ for pg_current_xact_id(); we target 14+.

CREATE TYPE entry_direction AS ENUM ('debit', 'credit');


-- ---------------------------------------------------------------------------
-- accounts
-- ---------------------------------------------------------------------------

CREATE TABLE accounts (
    id       UUID PRIMARY KEY,
    name     TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
    currency TEXT NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),

    -- Invariant #2, enforced by construction. See the concurrency note on
    -- `transfer` in src/db/mod.rs: transfers mutate this column with
    -- `UPDATE ... SET balance = balance - $1`, which takes a row-level write
    -- lock, so concurrent writers to the same account serialize and a negative
    -- balance is rejected by the database rather than by a racy read-then-check.
    --
    -- CAVEAT (deliberate, and only safe because this ledger is closed): a hard
    -- CHECK makes a negative balance *unrepresentable*. That is correct here
    -- because every balance change originates from an internally-authorized
    -- transfer inside the same DB transaction — there is no settlement
    -- estimate, chargeback, or provider correction that can force a negative
    -- position on us after the fact. The day this ledger ingests an external
    -- money source, this constraint must be relaxed to
    -- representable-but-monitored, because a system that cannot represent the
    -- state it is forced into will either abort mid-flow or clamp to zero, and
    -- clamping mints money. `allows_negative_balance` is the escape hatch.
    balance  NUMERIC(20, 8) NOT NULL DEFAULT 0,

    -- Funding / equity accounts. Invariants #2 and #3 are jointly satisfiable
    -- only if at least one account may hold a credit-normal (negative)
    -- position; otherwise the only reachable state is all-balances-zero and no
    -- money can ever enter the system. Customer accounts take the default.
    allows_negative_balance BOOLEAN NOT NULL DEFAULT FALSE,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT accounts_balance_non_negative
        CHECK (allows_negative_balance OR balance >= 0),

    -- Target for the composite foreign key from entries: lets the database
    -- guarantee an entry's currency can never diverge from its account's.
    CONSTRAINT accounts_id_currency_key UNIQUE (id, currency)
);


-- ---------------------------------------------------------------------------
-- transactions
-- ---------------------------------------------------------------------------

CREATE TABLE transactions (
    id UUID PRIMARY KEY,

    -- The idempotency barrier. This UNIQUE index is the serialization point
    -- for concurrent replays: a duplicate INSERT blocks on the index until the
    -- winner commits, then fails with 23505. The loser never reaches the entry
    -- INSERTs, so a replay cannot produce duplicate entries (invariant #4).
    idempotency_key TEXT NOT NULL UNIQUE,

    -- SHA-256 over the canonicalized request payload. Same key + same payload
    -- replays the original result; same key + different payload is a 409.
    request_hash BYTEA NOT NULL CHECK (octet_length(request_hash) = 32),

    description TEXT NOT NULL,

    -- Used by entries_same_xact_trigger below to prove that a transaction's
    -- entries were written in the same database transaction that created this
    -- row. Without it, the deferred balance check could be satisfied by
    -- appending a *separately* balanced pair of entries to an old transaction
    -- later, which would silently violate immutability of the transaction's
    -- meaning while passing every other check.
    created_xid XID8 NOT NULL DEFAULT pg_current_xact_id(),

    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);


-- ---------------------------------------------------------------------------
-- entries — the append-only log of record
-- ---------------------------------------------------------------------------

CREATE TABLE entries (
    id             UUID PRIMARY KEY,
    transaction_id UUID NOT NULL REFERENCES transactions (id),
    account_id     UUID NOT NULL,
    direction      entry_direction NOT NULL,

    -- Always positive; the sign lives in `direction`, never in the amount.
    amount NUMERIC(20, 8) NOT NULL CHECK (amount > 0),

    -- Denormalized from the account so the composite FK below can enforce
    -- that they agree, and so the per-transaction single-currency check can
    -- run without joining accounts.
    currency TEXT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT entries_account_currency_fkey
        FOREIGN KEY (account_id, currency)
        REFERENCES accounts (id, currency)
);

CREATE INDEX entries_transaction_id_idx ON entries (transaction_id);
CREATE INDEX entries_account_id_idx ON entries (account_id, created_at);


-- ---------------------------------------------------------------------------
-- Invariant #1, enforced at COMMIT
-- ---------------------------------------------------------------------------
--
-- A per-transaction sum spans rows, so it cannot be a row-level CHECK. A
-- DEFERRABLE INITIALLY DEFERRED constraint trigger runs at COMMIT, once every
-- entry of the transaction is present, and aborts the whole database
-- transaction if the postings do not balance.

CREATE FUNCTION assert_transaction_balanced() RETURNS TRIGGER AS $$
DECLARE
    net             NUMERIC;
    entry_count     INTEGER;
    currency_count  INTEGER;
BEGIN
    SELECT
        COALESCE(SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END), 0),
        COUNT(*),
        COUNT(DISTINCT currency)
    INTO net, entry_count, currency_count
    FROM entries
    WHERE transaction_id = NEW.transaction_id;

    IF entry_count < 2 THEN
        RAISE EXCEPTION
            'transaction % has % entries; a transaction requires at least 2',
            NEW.transaction_id, entry_count
            USING ERRCODE = 'check_violation',
                  CONSTRAINT = 'transaction_min_two_entries';
    END IF;

    IF currency_count <> 1 THEN
        RAISE EXCEPTION
            'transaction % mixes % currencies; all entries must share one',
            NEW.transaction_id, currency_count
            USING ERRCODE = 'check_violation',
                  CONSTRAINT = 'transaction_single_currency';
    END IF;

    IF net <> 0 THEN
        RAISE EXCEPTION
            'transaction % does not balance: debits - credits = %',
            NEW.transaction_id, net
            USING ERRCODE = 'check_violation',
                  CONSTRAINT = 'transaction_balanced';
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER entries_balanced_trigger
    AFTER INSERT ON entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION assert_transaction_balanced();


-- Entries may only be written in the same database transaction that created
-- their parent transaction row. This is what turns invariant #1 from "checked"
-- into "unrepresentable": there is no later moment at which a transaction's
-- set of entries can be extended.
CREATE FUNCTION assert_entry_in_parent_xact() RETURNS TRIGGER AS $$
DECLARE
    parent_xid XID8;
BEGIN
    SELECT created_xid INTO parent_xid
    FROM transactions
    WHERE id = NEW.transaction_id;

    IF parent_xid IS NULL THEN
        RAISE EXCEPTION 'entry references unknown transaction %', NEW.transaction_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    IF parent_xid <> pg_current_xact_id() THEN
        RAISE EXCEPTION
            'entries for transaction % must be written in the same database transaction that created it',
            NEW.transaction_id
            USING ERRCODE = 'check_violation',
                  CONSTRAINT = 'entries_written_with_parent';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entries_same_xact_trigger
    BEFORE INSERT ON entries
    FOR EACH ROW EXECUTE FUNCTION assert_entry_in_parent_xact();


-- ---------------------------------------------------------------------------
-- Immutability
-- ---------------------------------------------------------------------------
--
-- Corrections are new reversal transactions, never edits. Enforced with
-- triggers rather than only with GRANTs so that the rule travels with the
-- schema and holds even for a superuser connection.

CREATE FUNCTION reject_mutation() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        'ledger is append-only: % on % is forbidden; record a reversal instead',
        TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER entries_immutable_trigger
    BEFORE UPDATE OR DELETE ON entries
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();

CREATE TRIGGER transactions_immutable_trigger
    BEFORE UPDATE OR DELETE ON transactions
    FOR EACH ROW EXECUTE FUNCTION reject_mutation();

-- Accounts are a special case: `balance` is a mutable cache, everything else
-- about an account is immutable, and accounts are never deleted (a deleted
-- account would orphan history and break invariant #3).
CREATE FUNCTION reject_account_mutation() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'accounts are permanent: DELETE on accounts is forbidden'
            USING ERRCODE = 'restrict_violation';
    END IF;

    IF NEW.id <> OLD.id
        OR NEW.name <> OLD.name
        OR NEW.currency <> OLD.currency
        OR NEW.allows_negative_balance <> OLD.allows_negative_balance
        OR NEW.created_at <> OLD.created_at
    THEN
        RAISE EXCEPTION
            'only accounts.balance is mutable; account identity and terms are fixed at creation'
            USING ERRCODE = 'restrict_violation';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER accounts_immutable_trigger
    BEFORE UPDATE OR DELETE ON accounts
    FOR EACH ROW EXECUTE FUNCTION reject_account_mutation();


-- ---------------------------------------------------------------------------
-- Post-factum controls
-- ---------------------------------------------------------------------------

-- Balances as derived from the log of record. `accounts.balance` must always
-- agree with this view; any divergence means the cache has drifted and the
-- log wins.
CREATE VIEW account_balances_derived AS
SELECT
    a.id AS account_id,
    a.currency,
    COALESCE(
        SUM(CASE WHEN e.direction = 'debit' THEN e.amount ELSE -e.amount END),
        0
    )::NUMERIC(20, 8) AS balance
FROM accounts a
LEFT JOIN entries e ON e.account_id = a.id
GROUP BY a.id, a.currency;


-- The whole invariant suite as a single query, so reconciliation is a
-- first-class object in the schema rather than an assertion that only exists
-- inside the test suite. Intended to be run as a periodic sweep in production
-- and asserted after every property-test case.
CREATE FUNCTION ledger_check_invariants()
RETURNS TABLE (invariant TEXT, ok BOOLEAN, detail TEXT) AS $$
BEGIN
    -- Invariant #3: the system as a whole nets to zero.
    RETURN QUERY
    SELECT
        'global_sum_zero'::TEXT,
        COALESCE(SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END), 0) = 0,
        format('net = %s',
               COALESCE(SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END), 0))
    FROM entries;

    -- Invariant #1, verified independently of the commit-time trigger.
    RETURN QUERY
    WITH bad AS (
        SELECT transaction_id
        FROM entries
        GROUP BY transaction_id
        HAVING SUM(CASE WHEN direction = 'debit' THEN amount ELSE -amount END) <> 0
    )
    SELECT 'per_transaction_balanced'::TEXT,
           COUNT(*) = 0,
           format('%s unbalanced transaction(s)', COUNT(*))
    FROM bad;

    RETURN QUERY
    WITH bad AS (
        SELECT transaction_id
        FROM entries
        GROUP BY transaction_id
        HAVING COUNT(*) < 2 OR COUNT(DISTINCT currency) <> 1
    )
    SELECT 'transaction_shape'::TEXT,
           COUNT(*) = 0,
           format('%s transaction(s) with <2 entries or mixed currency', COUNT(*))
    FROM bad;

    -- Every transaction row has entries at all (no empty transactions).
    RETURN QUERY
    SELECT 'no_empty_transactions'::TEXT,
           COUNT(*) = 0,
           format('%s transaction(s) with no entries', COUNT(*))
    FROM transactions t
    WHERE NOT EXISTS (SELECT 1 FROM entries e WHERE e.transaction_id = t.id);

    -- The materialized cache still agrees with the log.
    RETURN QUERY
    WITH bad AS (
        SELECT a.id
        FROM accounts a
        JOIN account_balances_derived d ON d.account_id = a.id
        WHERE a.balance <> d.balance
    )
    SELECT 'materialized_balance_matches_entries'::TEXT,
           COUNT(*) = 0,
           format('%s account(s) drifted from the entry log', COUNT(*))
    FROM bad;

    -- Invariant #2, verified independently of the CHECK constraint.
    RETURN QUERY
    SELECT 'no_unauthorized_negative_balance'::TEXT,
           COUNT(*) = 0,
           format('%s account(s) negative without allows_negative_balance', COUNT(*))
    FROM accounts
    WHERE balance < 0 AND NOT allows_negative_balance;

    -- Invariant #4: an idempotency key maps to exactly one transaction. The
    -- UNIQUE index guarantees this; checked here so the sweep is self-contained.
    RETURN QUERY
    SELECT 'idempotency_keys_unique'::TEXT,
           COUNT(*) = 0,
           format('%s duplicated idempotency key(s)', COUNT(*))
    FROM (
        SELECT idempotency_key FROM transactions
        GROUP BY idempotency_key HAVING COUNT(*) > 1
    ) dup;
END;
$$ LANGUAGE plpgsql;
