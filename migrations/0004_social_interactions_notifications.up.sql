-- Epic 02, migration 0004: social graph, interactions, notifications, and
-- the Pubby-shaped generic interaction tables consumed by the
-- `ActivityPubStorage` adapter (epic 06).
--
-- `follows`/`follow_requests` carry the Pubby trait columns (inbox,
-- shared_inbox, actor_data) so the epic-06 adapter needs no second table.

CREATE TABLE follows (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Pubby trait columns (cached remote inbox/actor data).
    inbox             TEXT NOT NULL DEFAULT '',
    shared_inbox      TEXT NOT NULL DEFAULT '',
    actor_data        JSONB NOT NULL DEFAULT '{}'::jsonb,
    show_reblogs      BOOLEAN NOT NULL DEFAULT true,
    notify            BOOLEAN NOT NULL DEFAULT false,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (account_id <> target_account_id),
    UNIQUE (account_id, target_account_id)
);
CREATE INDEX ix_follows_target ON follows(target_account_id);

-- Pending follows for locked accounts.
CREATE TABLE follow_requests (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    inbox             TEXT NOT NULL DEFAULT '',
    shared_inbox      TEXT NOT NULL DEFAULT '',
    actor_data        JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- The original Follow activity, kept for approval/rejection.
    activity          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_account_id)
);
CREATE INDEX ix_follow_requests_target ON follow_requests(target_account_id);

CREATE TABLE blocks (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_account_id)
);
CREATE INDEX ix_blocks_target ON blocks(target_account_id);

CREATE TABLE mutes (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    hide_notifications BOOLEAN NOT NULL DEFAULT true,
    expires_at        TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_account_id)
);

-- User-level domain blocks (block a whole remote domain).
CREATE TABLE account_domain_blocks (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    domain     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, domain)
);

-- Subscribe to a user's posts (BOOTSTRAP "Profile pages").
CREATE TABLE subscriptions (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_account_id)
);

-- Private notes about another user, visible only to the author.
CREATE TABLE account_notes (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    comment           TEXT NOT NULL DEFAULT '' CHECK (char_length(comment) <= 2000),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_account_id)
);

CREATE TABLE lists (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id     BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    title          TEXT NOT NULL CHECK (char_length(title) <= 500),
    exclusive      BOOLEAN NOT NULL DEFAULT false,
    replies_policy TEXT NOT NULL DEFAULT 'list' CHECK (replies_policy IN ('followed', 'list', 'none')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE list_accounts (
    list_id    BIGINT NOT NULL REFERENCES lists(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY (list_id, account_id)
);

CREATE TABLE favourites (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, status_id)
);
CREATE INDEX ix_favourites_status ON favourites(status_id);

CREATE TABLE bookmarks (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, status_id)
);

-- Custom emoji reactions (Pleroma/Akkoma style); `emoji` is either a
-- unicode character or a `:shortcode:`. `custom_emoji_id` is added in
-- migration 0007 once `custom_emojis` exists.
CREATE TABLE emoji_reactions (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    emoji      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, status_id, emoji)
);
CREATE INDEX ix_emoji_reactions_status ON emoji_reactions(status_id);

CREATE TABLE notifications (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id      BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    from_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    type            TEXT NOT NULL CHECK (type IN (
                        'follow', 'follow_request', 'mention', 'favourite',
                        'reblog', 'quote', 'poll', 'status', 'update',
                        'emoji_reaction', 'admin.sign_up', 'moderation_warning')),
    status_id       BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    read            BOOLEAN NOT NULL DEFAULT false,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_notifications_account_created ON notifications (account_id, created_at DESC);
-- Unread-notification counts (menu badge).
CREATE INDEX ix_notifications_unread ON notifications (account_id, created_at DESC) WHERE NOT read;

-- Generic Pubby-shaped interactions (webmention/AP-facing store used by
-- the epic-06 `ActivityPubStorage` adapter). The normalized tables above
-- (favourites, emoji_reactions, reply statuses) remain the API-facing
-- source of truth; the final mapping is epic 06's call.
CREATE TABLE interactions (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    source_actor_id  TEXT NOT NULL,
    target_resource  TEXT NOT NULL,
    interaction_type TEXT NOT NULL,
    activity_id      TEXT NOT NULL DEFAULT '',
    object_id        TEXT NOT NULL DEFAULT '',
    content          TEXT NOT NULL DEFAULT '',
    author_name      TEXT NOT NULL DEFAULT '',
    author_url       TEXT NOT NULL DEFAULT '',
    author_photo     TEXT NOT NULL DEFAULT '',
    published        TIMESTAMPTZ,
    status           TEXT NOT NULL DEFAULT 'confirmed',
    metadata         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at       TIMESTAMPTZ,
    updated_at       TIMESTAMPTZ,
    UNIQUE (source_actor_id, target_resource, interaction_type, object_id)
);
CREATE INDEX ix_interactions_target ON interactions(target_resource);
CREATE INDEX ix_interactions_object ON interactions(object_id);

CREATE TABLE interaction_mentions (
    interaction_id      BIGINT NOT NULL REFERENCES interactions(id) ON DELETE CASCADE,
    mentioned_actor_url TEXT NOT NULL,
    UNIQUE (interaction_id, mentioned_actor_url)
);
CREATE INDEX ix_interaction_mentions_actor ON interaction_mentions(mentioned_actor_url);
