-- Epic 12, migration 0014: posting — rendered HTML storage, scheduled
-- per-user deletion, media focus point, scheduled statuses with media
-- reservations, and durable status-creation idempotency.
--
-- `content` keeps the raw source text; `content_html` is the rendered
-- (sanitized) document both the REST entity and the AP object serve.
-- Remote statuses store `content_type='text/plain'` with the AP
-- `content` HTML in `content_html` (the CHECK governs local
-- composition, not stored remote data).

ALTER TABLE statuses ADD COLUMN content_html TEXT;
ALTER TABLE statuses ADD COLUMN scheduled_deletion_at TIMESTAMPTZ;
CREATE INDEX ix_statuses_scheduled_deletion ON statuses (scheduled_deletion_at)
    WHERE scheduled_deletion_at IS NOT NULL AND NOT tombstone;

-- Focal point for cropped previews; each coordinate in [-1, 1],
-- NULL = no focus set.
ALTER TABLE media_attachments
    ADD COLUMN focus_x REAL,
    ADD COLUMN focus_y REAL;
ALTER TABLE media_attachments
    ADD CONSTRAINT media_attachments_focus_x_range CHECK (focus_x BETWEEN -1 AND 1),
    ADD CONSTRAINT media_attachments_focus_y_range CHECK (focus_y BETWEEN -1 AND 1);

-- Validated create payloads parked until `scheduled_at`; media
-- ownership lives in `scheduled_status_media`, never in params.
CREATE TABLE scheduled_statuses (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    scheduled_at TIMESTAMPTZ NOT NULL,
    params       JSONB NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_scheduled_statuses_account ON scheduled_statuses (account_id, scheduled_at);

-- Media reserved by a scheduled post — the orphan cleanup never
-- reclaims an attachment referenced here. References move onto the
-- status (`media_attachments.status_id`) atomically on publish.
CREATE TABLE scheduled_status_media (
    scheduled_status_id  BIGINT NOT NULL REFERENCES scheduled_statuses(id) ON DELETE CASCADE,
    media_attachment_id  BIGINT NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    PRIMARY KEY (scheduled_status_id, media_attachment_id)
);
CREATE INDEX ix_scheduled_status_media_media ON scheduled_status_media (media_attachment_id);

-- Atomic, typed idempotency for status/scheduled-status creation.
-- A row reserves (account_id, idempotency_key) at request start;
-- `target_id` NULL means another request is in flight, a set
-- `target_id` + matching `fingerprint` replays the stored target, and
-- a different `fingerprint` is a 422.
CREATE TABLE status_idempotency_keys (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id       BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    idempotency_key  TEXT NOT NULL,
    target_type      TEXT NOT NULL CHECK (target_type IN ('status', 'scheduled_status')),
    target_id        BIGINT,
    fingerprint      TEXT NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, idempotency_key)
);
CREATE INDEX ix_status_idempotency_created ON status_idempotency_keys (created_at);
