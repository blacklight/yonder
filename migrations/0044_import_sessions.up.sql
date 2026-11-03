-- Migration 0044: resumable import sessions + per-item source mapping.
--
-- `archive_imports` grows the session fields the archive/API import
-- wizard needs: source provenance, immutable options, stage progress,
-- a frozen source cutoff, and pause/cancel/retry control. `file_key`
-- becomes optional because API-source imports have no uploaded file.
--
-- `import_items` is the source→destination mapping used for dedupe,
-- two-pass graph fix-up, resumable per-item progress, and crash
-- recovery. `statuses.original_id` stays the authored-copy dedupe key;
-- boosts keep their own source identity here.

ALTER TABLE archive_imports
    ADD COLUMN source_type      TEXT NOT NULL DEFAULT 'archive'
                                CHECK (source_type IN ('archive', 'api')),
    ADD COLUMN source_actor_uri TEXT,
    ADD COLUMN source_origin    TEXT,
    -- Immutable for the session's lifetime (category selection,
    -- limits, order, time range, footer, federate choice).
    ADD COLUMN options          JSONB NOT NULL DEFAULT '{}',
    ADD COLUMN stage            TEXT,
    -- Per-category cursors/counts + per-page resume offsets.
    ADD COLUMN category_state   JSONB NOT NULL DEFAULT '{}',
    -- Source boundary frozen at session creation.
    ADD COLUMN cutoff           TIMESTAMPTZ,
    ADD COLUMN cancel_requested BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN next_retry_at    TIMESTAMPTZ,
    ADD COLUMN summary          JSONB;

ALTER TABLE archive_imports ALTER COLUMN file_key DROP NOT NULL;

ALTER TABLE archive_imports DROP CONSTRAINT archive_imports_state_check;
ALTER TABLE archive_imports ADD CONSTRAINT archive_imports_state_check
    CHECK (state IN ('pending', 'running', 'paused', 'done', 'failed', 'cancelled'));

CREATE TABLE import_items (
    id               BIGINT PRIMARY KEY DEFAULT timestamp_id('import_items'),
    import_id        BIGINT NOT NULL REFERENCES archive_imports(id) ON DELETE CASCADE,
    account_id       BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- 'post' | 'reply' | 'boost' | 'bookmark' | 'like' | 'dm'
    -- | 'profile' | 'follow' | 'block' | 'mute' | 'observation'
    kind             TEXT NOT NULL,
    source_uri       TEXT NOT NULL,
    target_status_id BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    -- Non-status targets (favourite/bookmark/relationship row ids).
    target_id        BIGINT,
    state            TEXT NOT NULL DEFAULT 'pending'
                     CHECK (state IN ('pending', 'done', 'skipped', 'failed', 'unsupported')),
    error            TEXT,
    -- Original relation URIs for the two-pass graph fix-up.
    in_reply_to_uri  TEXT,
    quote_uri        TEXT,
    reblog_uri       TEXT,
    -- Source ordering metadata (page + position) for resume.
    ordering         BIGINT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, source_uri, kind)
);

CREATE INDEX ix_import_items_import ON import_items (import_id, state);
CREATE INDEX ix_import_items_reply ON import_items (in_reply_to_uri)
    WHERE in_reply_to_uri IS NOT NULL;
