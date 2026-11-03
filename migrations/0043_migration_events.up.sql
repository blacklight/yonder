-- Migration 0043: durable migration events + per-follower outcomes.
--
-- Move previously derived activity ids from `SELECT count(*) FROM
-- activities`, published opportunistically during `move_to`, and
-- advanced the follower cursor past failures — losing followers on
-- transient remote errors. `migration_events` is the durable workflow
-- record: minted snowflake id, state machine, frozen follower cutoff,
-- and a replay/dedupe key for inbound Moves.
--
-- `migration_follower_outcomes` records each follower's result so a
-- failed follow is retried on the next run instead of silently
-- skipped.

CREATE TABLE migration_events (
    id                  BIGINT PRIMARY KEY DEFAULT timestamp_id('migration_events'),
    source_account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Verified inbound Move activity identity — replay dedupe key.
    source_activity_id  TEXT,
    -- 'pending' → 'publishing' → 'migrating' → 'done';
    -- 'failed' is terminal-with-reporting (move_state shows the
    -- failed outcome counts); 'cancelled' is terminal.
    state               TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN (
                            'pending', 'publishing', 'migrating',
                            'done', 'failed', 'cancelled')),
    -- Follower-scan boundary frozen at event creation so new arrivals
    -- do not move the target during the migration.
    follower_cutoff     TIMESTAMPTZ NOT NULL DEFAULT now(),
    cursor              BIGINT NOT NULL DEFAULT 0,
    progress            JSONB NOT NULL DEFAULT '{}',
    error               TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at        TIMESTAMPTZ
);

-- At most one live event per (source, target) pair; a second move to
-- the same target is allowed once the first reaches a terminal state.
CREATE UNIQUE INDEX uix_migration_events_active
    ON migration_events (source_account_id, target_account_id)
    WHERE state IN ('pending', 'publishing', 'migrating');

-- Verified inbound activity identity — replays coalesce onto it.
CREATE UNIQUE INDEX uix_migration_events_activity
    ON migration_events (source_activity_id)
    WHERE source_activity_id IS NOT NULL;

CREATE INDEX ix_migration_events_origin ON migration_events (source_account_id);

CREATE TABLE migration_follower_outcomes (
    id                  BIGINT PRIMARY KEY
                        DEFAULT timestamp_id('migration_follower_outcomes'),
    event_id            BIGINT NOT NULL REFERENCES migration_events(id) ON DELETE CASCADE,
    follower_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- 'pending' → 'migrated' | 'skipped' (blocked/muted/locked-remote)
    -- | 'failed' (retried with backoff while next_retry_at is set;
    -- next_retry_at NULL marks a permanent, reported failure — never
    -- silently dropped).
    state               TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'migrated', 'failed', 'skipped')),
    attempts            INT NOT NULL DEFAULT 0,
    next_retry_at       TIMESTAMPTZ,
    error               TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_id, follower_account_id)
);

CREATE INDEX ix_migration_follower_pending
    ON migration_follower_outcomes (event_id, state)
    WHERE state IN ('pending', 'failed');
