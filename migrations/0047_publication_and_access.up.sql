-- Migration 0047: durable import publication work, `move`
-- notifications, and the restricted session access class.
--
-- `publication_work` is the durable per-item federation record for
-- imports that chose `federate`: deterministic activity ids and a
-- pending-work table bridging "status committed" and "delivery job
-- enqueued", so a crash between the two resumes instead of losing or
-- duplicating the publication.
--
-- `notifications` gains the Akkoma-style `move` type plus the
-- migration event and target account references. The dedupe index now
-- keys relationship-free notifications on their event as well as
-- status, so repeat migrations to *different* destinations notify
-- again while replays of the same event do not.
--
-- `access_class` marks restricted (export-only) credentials:
-- `oauth_access_tokens` issued to suspended users and the browser
-- `sessions` behind them. Ordinary token resolution rejects anything
-- not 'ordinary' regardless of the scopes string.

CREATE TABLE publication_work (
    id            BIGINT PRIMARY KEY DEFAULT timestamp_id('publication_work'),
    import_id     BIGINT REFERENCES archive_imports(id) ON DELETE CASCADE,
    import_item_id BIGINT REFERENCES import_items(id) ON DELETE CASCADE,
    status_id     BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    activity_type TEXT NOT NULL,
    -- Deterministic activity identity — retry-safe.
    activity_id   TEXT NOT NULL UNIQUE,
    -- Frozen publish inputs (activity document, signing username,
    -- and any delivery metadata the activity type needs).
    payload       JSONB NOT NULL DEFAULT '{}',
    state         TEXT NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending', 'delivered', 'failed')),
    attempts      INT NOT NULL DEFAULT 0,
    next_retry_at TIMESTAMPTZ,
    error         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ix_publication_work_pending
    ON publication_work (state, next_retry_at) WHERE state = 'pending';

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning',
        'admin.report', 'move'));

ALTER TABLE notifications
    ADD COLUMN migration_event_id BIGINT REFERENCES migration_events(id) ON DELETE CASCADE,
    ADD COLUMN target_account_id  BIGINT REFERENCES accounts(id) ON DELETE CASCADE;

DROP INDEX IF EXISTS ix_notifications_dedupe;
CREATE UNIQUE INDEX ix_notifications_dedupe
    ON notifications (account_id, from_account_id, type,
                      COALESCE(status_id, migration_event_id, 0));

ALTER TABLE oauth_access_tokens
    ADD COLUMN access_class TEXT NOT NULL DEFAULT 'ordinary'
    CHECK (access_class IN ('ordinary', 'restricted'));

ALTER TABLE sessions
    ADD COLUMN access_class TEXT NOT NULL DEFAULT 'ordinary'
    CHECK (access_class IN ('ordinary', 'restricted'));
