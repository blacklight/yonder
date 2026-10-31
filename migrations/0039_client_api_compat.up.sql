-- Epic 26, migration 0039: storage for the Mastodon/Pleroma client
-- API surface that mobile apps (Moshidon, the official Mastodon app)
-- hit on every session — timeline markers, push subscriptions, edit
-- history, featured tags, conversation mutes, notification policy.
--
-- `markers` — /api/v1/markers: per-account last-read position for
-- each named timeline (`home`, `notifications`). `version` is a
-- monotonically increasing counter the client echoes back.
CREATE TABLE markers (
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    timeline     TEXT NOT NULL CHECK (timeline IN ('home', 'notifications')),
    last_read_id BIGINT NOT NULL,
    version      INT NOT NULL DEFAULT 0,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, timeline)
);

-- `web_push_subscriptions` already exists (migration 0007):
-- account_id-keyed, endpoint globally unique, `data` JSONB carries
-- the alerts/policy map — `/api/v1/push/subscription` rides on it.

-- `status_edits` — /api/v1/statuses/{id}/history: a full snapshot of
-- every superseded version. Written by `edit_status` before each
-- update; the live `statuses` row is always the newest version.
-- `version_at` is when the version was authored (post's created_at
-- for the original, the edit time for later versions).
CREATE TABLE status_edits (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    status_id    BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    content      TEXT NOT NULL,
    content_html TEXT,
    spoiler_text TEXT,
    sensitive    BOOLEAN NOT NULL DEFAULT false,
    poll         JSONB,
    media        JSONB,
    version_at   TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_status_edits_status ON status_edits(status_id, version_at);

-- `featured_tags` — /api/v1/featured_tags: hashtags a user pins to
-- their profile (Mastodon's per-account featured list).
CREATE TABLE featured_tags (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    tag_id     BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, tag_id)
);
CREATE INDEX ix_featured_tags_tag ON featured_tags(tag_id);

-- `conversation_mutes` — /api/v1/statuses/{id}/mute: mutes the whole
-- thread. `root_status_id` is the thread root (self for top-level
-- posts), resolved once at mute time.
CREATE TABLE conversation_mutes (
    account_id     BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    root_status_id BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, root_status_id)
);

-- `notification_policies` — /api/v1/notifications/policy (Mastodon
-- 4.3+): per-account flags. Stored and echoed back; the request
-- inbox they feed is not implemented yet, so the flags are
-- informational — `GET /api/v1/notifications/requests` always
-- returns an empty page.
CREATE TABLE notification_policies (
    account_id               BIGINT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    filter_not_followers     BOOLEAN NOT NULL DEFAULT false,
    filter_not_following     BOOLEAN NOT NULL DEFAULT false,
    filter_new_accounts      BOOLEAN NOT NULL DEFAULT false,
    filter_private_mentions  BOOLEAN NOT NULL DEFAULT false,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);
