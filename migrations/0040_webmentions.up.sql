-- Migration 0040: Webmentions (send + receive).
--
-- `webmentions` is the delivery ledger backing the `webmentions-sdk`
-- storage trait: one row per (source, target, direction) pair.
-- Incoming rows additionally link the materialized pseudo-actor
-- (`account_id`) and the reply status it authored (`status_id`);
-- outgoing rows track which targets a local status's links were
-- delivered to so edits and deletions can retract them.

CREATE TABLE webmentions (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- The page that mentions the target.
    source           TEXT NOT NULL,
    -- The page that was mentioned (a local status permalink for
    -- incoming rows, a remote URL for outgoing ones).
    target           TEXT NOT NULL,
    direction        TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
    status           TEXT NOT NULL DEFAULT 'confirmed'
                     CHECK (status IN ('pending', 'confirmed', 'deleted')),
    mention_type     TEXT NOT NULL DEFAULT 'unknown',
    mention_type_raw TEXT,
    title            TEXT,
    excerpt          TEXT,
    content          TEXT,
    author_name      TEXT,
    author_url       TEXT,
    author_photo     TEXT,
    published        TIMESTAMPTZ,
    metadata         JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- Incoming: the pseudo-actor and the reply status it authored.
    -- Outgoing: NULL (the originating status is recoverable from the
    -- source URL's id suffix).
    status_id        BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    account_id       BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX uix_webmentions_source_target_direction
    ON webmentions (source, target, direction);
-- Every incoming mention of one of our permalinks (profile lookups).
CREATE INDEX ix_webmentions_target ON webmentions (target)
    WHERE direction = 'incoming';
-- Outgoing ledger per local status source URL.
CREATE INDEX ix_webmentions_outgoing_source ON webmentions (source)
    WHERE direction = 'outgoing';
