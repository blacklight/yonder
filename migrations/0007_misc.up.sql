-- Epic 02, migration 0007: custom emoji, filters, invites, feed cache,
-- scheduled jobs and WebPush subscriptions.

CREATE TABLE custom_emojis (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    shortcode        TEXT NOT NULL CHECK (shortcode ~ '^[a-zA-Z0-9_]+$' AND char_length(shortcode) <= 100),
    -- NULL = local emoji; remote emojis are cached here too.
    domain           TEXT,
    image_path       TEXT,
    remote_url       TEXT,
    disabled         BOOLEAN NOT NULL DEFAULT false,
    visible_in_picker BOOLEAN NOT NULL DEFAULT true,
    category         TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX uix_custom_emojis_shortcode_domain
    ON custom_emojis (shortcode, COALESCE(domain, ''));

ALTER TABLE emoji_reactions
    ADD COLUMN custom_emoji_id BIGINT REFERENCES custom_emojis(id) ON DELETE SET NULL;

CREATE TABLE filters (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    title      TEXT NOT NULL CHECK (char_length(title) <= 500),
    context    TEXT[] NOT NULL DEFAULT '{home}' CHECK (context <@ '{home,notifications,public,thread,account}'),
    action     TEXT NOT NULL DEFAULT 'warn' CHECK (action IN ('warn', 'hide', 'blur')),
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE filter_keywords (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    filter_id  BIGINT NOT NULL REFERENCES filters(id) ON DELETE CASCADE,
    keyword    TEXT NOT NULL CHECK (char_length(keyword) <= 500),
    whole_word BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_filter_keywords_filter ON filter_keywords(filter_id);

CREATE TABLE filter_statuses (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    filter_id  BIGINT NOT NULL REFERENCES filters(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (filter_id, status_id)
);

CREATE TABLE invites (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    code                 TEXT NOT NULL UNIQUE,
    created_by_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    max_uses             INT,
    uses                 INT NOT NULL DEFAULT 0,
    expires_at           TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Cached RSS/Atom feed entries (epic 19): one row per status included in a
-- profile/hashtag/list feed.
CREATE TABLE feed_items (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    feed_type    TEXT NOT NULL CHECK (feed_type IN ('profile', 'hashtag', 'list')),
    -- Account id, tag name or list id, as text.
    feed_key     TEXT NOT NULL,
    status_id    BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (feed_type, feed_key, status_id)
);
CREATE INDEX ix_feed_items_feed ON feed_items (feed_type, feed_key, published_at DESC);

-- Generic scheduler rows consumed by the epic-03 scheduler: scheduled
-- posts (job_type = 'post'), cron-driven maintenance, etc.
CREATE TABLE scheduled_jobs (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_type   TEXT NOT NULL,
    payload    JSONB NOT NULL DEFAULT '{}'::jsonb,
    run_at     TIMESTAMPTZ NOT NULL,
    state      TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'running', 'done', 'failed', 'cancelled')),
    attempts   INT NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_scheduled_jobs_state_run ON scheduled_jobs (state, run_at);

CREATE TABLE web_push_subscriptions (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    endpoint   TEXT NOT NULL UNIQUE,
    p256dh     TEXT NOT NULL,
    auth       TEXT NOT NULL,
    data       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_web_push_subscriptions_account ON web_push_subscriptions(account_id);
