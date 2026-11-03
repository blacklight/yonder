-- Migration 0046: historical received-interaction overlays and
-- private imported messages.
--
-- Remote APIs can report that an old post received likes/boosts/
-- quotes/replies without exposing every actor, activity id or
-- timestamp. `historical_observations` records what was observed —
-- provenance-labelled, linked to the imported local copy, never
-- fabricated as live local interactions on the new object. Aggregate
-- counts the source reported without an enumerable list live on the
-- status's existing `remote_*_count` columns (migration 0042), which
-- already render as GREATEST(local, remote) without mutating
-- transactional counters.
--
-- `private_history_messages` stores received direct/private messages
-- whose original audience ordinary status visibility cannot safely
-- represent: original sender/recipients and provenance, owner-only.
-- Sender identity is never rewritten.

CREATE TABLE historical_observations (
    id              BIGINT PRIMARY KEY
                    DEFAULT timestamp_id('historical_observations'),
    owner_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Imported local copy the observation is attached to (nullable:
    -- observations may precede graph fix-up).
    status_id       BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    source_post_uri TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('like', 'boost', 'quote', 'reply')),
    -- Original actor/activity/object URIs — null when the source did
    -- not expose them (unknown, never invented).
    actor_uri       TEXT,
    activity_uri    TEXT,
    object_uri      TEXT,
    -- When the observation was recorded vs the source's own activity
    -- time (null when unknown — never disguised as import time).
    observed_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    original_at     TIMESTAMPTZ,
    source_origin   TEXT,
    visibility      TEXT NOT NULL DEFAULT 'public'
                    CHECK (visibility IN (
                        'public', 'unlisted', 'followers_only', 'private', 'direct')),
    -- Deterministic dedupe key so retries/replays coalesce.
    dedupe_key      TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, dedupe_key)
);

CREATE INDEX ix_historical_observations_status
    ON historical_observations (status_id) WHERE status_id IS NOT NULL;
CREATE INDEX ix_historical_observations_owner
    ON historical_observations (owner_id, kind);

CREATE TABLE private_history_messages (
    id                BIGINT PRIMARY KEY
                      DEFAULT timestamp_id('private_history_messages'),
    owner_id          BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    import_id         BIGINT REFERENCES archive_imports(id) ON DELETE CASCADE,
    -- Provenance: the original sender is kept as an AP URI; the local
    -- account link is advisory only.
    sender_uri        TEXT NOT NULL,
    sender_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    recipient_uris    JSONB NOT NULL DEFAULT '[]',
    body              TEXT,
    media_keys        JSONB NOT NULL DEFAULT '[]',
    source_uri        TEXT NOT NULL,
    source_origin     TEXT,
    original_at       TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, source_uri)
);

CREATE INDEX ix_private_history_owner
    ON private_history_messages (owner_id, original_at);
