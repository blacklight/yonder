-- Epic 02, migration 0003: statuses, polls, media attachments, hashtags,
-- link previews and pinned posts.
--
-- Reply/boost/quote are self-references on `statuses`; a boost is a status
-- row with `reblog_of_id` set (Mastodon semantics), a quote carries
-- `quote_of_id`.

CREATE TABLE statuses (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id           BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    content              TEXT NOT NULL DEFAULT '' CHECK (char_length(content) <= 20000),
    content_type         TEXT NOT NULL DEFAULT 'text/plain' CHECK (content_type IN ('text/plain', 'text/markdown')),
    language             TEXT,
    visibility           TEXT NOT NULL DEFAULT 'public' CHECK (visibility IN ('public', 'unlisted', 'followers_only', 'private', 'direct')),
    -- Content warning text.
    spoiler_text         TEXT CHECK (char_length(spoiler_text) <= 2000),
    sensitive            BOOLEAN NOT NULL DEFAULT false,
    local                BOOLEAN NOT NULL DEFAULT true,
    -- ActivityPub identifier URI, unique per object.
    uri                  TEXT UNIQUE,
    url                  TEXT,
    in_reply_to_id       BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    in_reply_to_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    reblog_of_id         BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    quote_of_id          BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    poll_id              BIGINT,
    replies_count        INT NOT NULL DEFAULT 0,
    reblogs_count        INT NOT NULL DEFAULT 0,
    favourites_count     INT NOT NULL DEFAULT 0,
    quotes_count         INT NOT NULL DEFAULT 0,
    edited_at            TIMESTAMPTZ,
    -- Federation deletes keep a tombstone row so remote instances can be
    -- told the object is gone.
    tombstone            BOOLEAN NOT NULL DEFAULT false,
    -- Import support: preserve the source archive's id/timestamp.
    original_id          TEXT,
    original_created_at  TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Profile timelines and fan-out.
CREATE INDEX ix_statuses_account_created ON statuses (account_id, created_at DESC);
-- Local/federated timeline scans.
CREATE INDEX ix_statuses_public_created ON statuses (created_at DESC)
    WHERE visibility IN ('public', 'unlisted') AND NOT tombstone;
CREATE INDEX ix_statuses_in_reply_to ON statuses(in_reply_to_id);
CREATE INDEX ix_statuses_reblog_of ON statuses(reblog_of_id);
CREATE INDEX ix_statuses_quote_of ON statuses(quote_of_id);
-- ILIKE/trigram search (epic 17).
CREATE INDEX ix_statuses_content_trgm ON statuses USING gin (content gin_trgm_ops);
CREATE INDEX ix_statuses_spoiler_trgm ON statuses USING gin (spoiler_text gin_trgm_ops);

CREATE TABLE polls (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    status_id  BIGINT NOT NULL UNIQUE REFERENCES statuses(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ,
    multiple   BOOLEAN NOT NULL DEFAULT false,
    -- Hide vote counts until the poll expires.
    hide_totals BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE poll_options (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    poll_id     BIGINT NOT NULL REFERENCES polls(id) ON DELETE CASCADE,
    idx         INT NOT NULL,
    title       TEXT NOT NULL CHECK (char_length(title) <= 500),
    votes_count INT NOT NULL DEFAULT 0,
    UNIQUE (poll_id, idx)
);

CREATE TABLE poll_votes (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    poll_id    BIGINT NOT NULL REFERENCES polls(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    option_idx INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Multiple-choice polls allow several options per voter.
    UNIQUE (poll_id, account_id, option_idx)
);
CREATE INDEX ix_poll_votes_account ON poll_votes(account_id);

ALTER TABLE statuses
    ADD CONSTRAINT fk_statuses_poll FOREIGN KEY (poll_id) REFERENCES polls(id) ON DELETE SET NULL;

CREATE TABLE status_mentions (
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY (status_id, account_id)
);
CREATE INDEX ix_status_mentions_account ON status_mentions(account_id);

CREATE TABLE tags (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE CHECK (char_length(name) <= 200),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE status_tags (
    status_id BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    tag_id    BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (status_id, tag_id)
);
CREATE INDEX ix_status_tags_tag ON status_tags(tag_id);

-- Followed hashtags (BOOTSTRAP "Menu").
CREATE TABLE tag_follows (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    tag_id     BIGINT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, tag_id)
);

CREATE TABLE media_attachments (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id  BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id   BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    type        TEXT NOT NULL CHECK (type IN ('image', 'video', 'audio', 'document')),
    file_path   TEXT,
    remote_url  TEXT,
    content_type TEXT,
    file_size   BIGINT,
    -- Alt text.
    description TEXT CHECK (char_length(description) <= 5000),
    blurhash    TEXT,
    width       INT,
    height      INT,
    exif_stripped BOOLEAN NOT NULL DEFAULT false,
    -- Post-processing (transcode/EXIF strip) completed.
    processed   BOOLEAN NOT NULL DEFAULT false,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (file_path IS NOT NULL OR remote_url IS NOT NULL)
);
CREATE INDEX ix_media_attachments_status ON media_attachments(status_id);
CREATE INDEX ix_media_attachments_account ON media_attachments (account_id, created_at DESC);

-- Open Graph link previews (epic 12).
CREATE TABLE preview_cards (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    status_id    BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    url          TEXT NOT NULL,
    title        TEXT NOT NULL DEFAULT '',
    description  TEXT NOT NULL DEFAULT '',
    image_path   TEXT,
    author_name  TEXT NOT NULL DEFAULT '',
    author_url   TEXT NOT NULL DEFAULT '',
    provider_name TEXT NOT NULL DEFAULT '',
    provider_url TEXT NOT NULL DEFAULT '',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (status_id, url)
);

-- Pinned posts (max enforced by content.max_pinned_posts).
CREATE TABLE status_pins (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    idx        INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, status_id)
);
