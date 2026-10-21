-- Epic 02, migration 0002: actors (local + remote), local users, and the
-- credentials/auth surface (2FA, OAuth, sessions).
--
-- `accounts` holds every ActivityPub actor; `domain IS NULL` marks local
-- actors. `users` holds local credentials and per-user preferences, 1:1
-- with a local account.

CREATE TABLE accounts (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- NULL = local actor; remote actors are cached here too.
    domain              TEXT,
    username            TEXT NOT NULL CHECK (char_length(username) <= 200),
    -- ActivityPub identifier URI, unique across local and remote actors.
    actor_id            TEXT NOT NULL UNIQUE,
    url                 TEXT,
    inbox               TEXT,
    shared_inbox        TEXT,
    outbox_url          TEXT,
    public_key          TEXT,
    -- Cached raw actor document (Pubby `ActivityPubStorage` shape).
    actor_data          JSONB NOT NULL DEFAULT '{}'::jsonb,
    display_name        TEXT CHECK (char_length(display_name) <= 500),
    bio                 TEXT CHECK (char_length(bio) <= 20000),
    -- Array of {name, value, verified_at}; count/lengths enforced by the
    -- validation layer (content.max_profile_fields etc.).
    profile_fields      JSONB NOT NULL DEFAULT '[]'::jsonb,
    avatar_path         TEXT,
    banner_path         TEXT,
    avatar_remote_url   TEXT,
    banner_remote_url   TEXT,
    -- Manual follow approval.
    locked              BOOLEAN NOT NULL DEFAULT false,
    bot                 BOOLEAN NOT NULL DEFAULT false,
    discoverable        BOOLEAN NOT NULL DEFAULT true,
    hide_network        BOOLEAN NOT NULL DEFAULT false,
    -- Include profile in the instance's profile directory.
    directory_opt_in    BOOLEAN NOT NULL DEFAULT true,
    default_visibility  TEXT NOT NULL DEFAULT 'public' CHECK (default_visibility IN (
                            'public', 'unlisted', 'followers_only', 'private', 'direct')),
    default_language    TEXT,
    role                TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('user', 'moderator', 'admin')),
    moved_to_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    -- Best-effort remote counters (epic 14).
    followers_count     INT NOT NULL DEFAULT 0,
    following_count     INT NOT NULL DEFAULT 0,
    statuses_count      INT NOT NULL DEFAULT 0,
    suspended_at        TIMESTAMPTZ,
    limited_at          TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_activity_at    TIMESTAMPTZ
);

-- Plain UNIQUE treats NULLs as distinct, so local actors (domain NULL)
-- would not collide; the expression index makes (username, domain) unique
-- with local actors keyed on ''.
CREATE UNIQUE INDEX uix_accounts_username_domain
    ON accounts (username, COALESCE(domain, ''));
CREATE INDEX ix_accounts_domain ON accounts(domain);
CREATE INDEX ix_accounts_actor_data_trgm
    ON accounts USING gin ((COALESCE(display_name, '') || ' ' || username) gin_trgm_ops);

CREATE TABLE users (
    id                             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id                     BIGINT NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
    email                          TEXT UNIQUE CHECK (char_length(email) <= 500),
    email_verified_at              TIMESTAMPTZ,
    -- argon2id hash; semantics land in epic 07.
    password_hash                  TEXT,
    -- Approval-required registrations (auth.registration_mode).
    approved_at                    TIMESTAMPTZ,
    registration_reason            TEXT CHECK (char_length(registration_reason) <= 10000),
    require_password_reset         BOOLEAN NOT NULL DEFAULT false,
    -- Email notification preferences (BOOTSTRAP "Users").
    email_notif_follow             BOOLEAN NOT NULL DEFAULT false,
    email_notif_follow_request     BOOLEAN NOT NULL DEFAULT true,
    email_notif_reblog             BOOLEAN NOT NULL DEFAULT false,
    email_notif_favourite          BOOLEAN NOT NULL DEFAULT false,
    email_notif_mention            BOOLEAN NOT NULL DEFAULT true,
    email_notif_quote              BOOLEAN NOT NULL DEFAULT true,
    -- Local preferences.
    filter_languages               TEXT[] NOT NULL DEFAULT '{}',
    locale                         TEXT,
    theme                          TEXT,
    accent                         TEXT,
    -- CW/sensitive content: false = show warning, expand on click (default);
    -- true = always expand.
    cw_always_expand               BOOLEAN NOT NULL DEFAULT false,
    show_app_display               BOOLEAN NOT NULL DEFAULT true,
    default_content_type           TEXT NOT NULL DEFAULT 'text/plain' CHECK (default_content_type IN ('text/plain', 'text/markdown')),
    scheduled_deletion_days        INT,
    include_public_posts_in_search BOOLEAN NOT NULL DEFAULT true,
    -- Include profile page in search-engine results.
    indexable                      BOOLEAN NOT NULL DEFAULT true,
    created_at                     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- URIs of accounts this account has moved from (Mastodon move semantics).
CREATE TABLE account_aliases (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    uri        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, uri)
);

-- 2FA: TOTP (epic 08).
CREATE TABLE two_factor (
    user_id          BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    totp_secret      TEXT NOT NULL,
    enabled_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_verified_at TIMESTAMPTZ
);

-- 2FA: WebAuthn/YubiKey credentials (epic 08).
CREATE TABLE webauthn_credentials (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id       BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    credential_id BYTEA NOT NULL UNIQUE,
    public_key    BYTEA NOT NULL,
    name          TEXT NOT NULL DEFAULT '' CHECK (char_length(name) <= 500),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at  TIMESTAMPTZ
);
CREATE INDEX ix_webauthn_credentials_user ON webauthn_credentials(user_id);

-- 2FA: one-time backup codes (epic 08).
CREATE TABLE backup_codes (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash  TEXT NOT NULL,
    used_at    TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_backup_codes_user ON backup_codes(user_id);

CREATE TABLE oauth_applications (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name              TEXT NOT NULL CHECK (char_length(name) <= 500),
    client_id         TEXT NOT NULL UNIQUE,
    -- Stored hashed; the plaintext secret is only shown at creation (epic 07).
    client_secret_hash TEXT NOT NULL,
    redirect_uris     TEXT[] NOT NULL DEFAULT '{}',
    scopes            TEXT NOT NULL DEFAULT 'read',
    website           TEXT,
    confidential      BOOLEAN NOT NULL DEFAULT true,
    owner_account_id  BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The "authorized apps" list a user sees and can revoke.
CREATE TABLE oauth_authorizations (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id         BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    application_id  BIGINT NOT NULL REFERENCES oauth_applications(id) ON DELETE CASCADE,
    scopes          TEXT NOT NULL DEFAULT 'read',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, application_id)
);

CREATE TABLE oauth_access_tokens (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    application_id BIGINT REFERENCES oauth_applications(id) ON DELETE CASCADE,
    user_id        BIGINT REFERENCES users(id) ON DELETE CASCADE,
    token_hash     TEXT NOT NULL UNIQUE,
    scopes         TEXT NOT NULL DEFAULT 'read',
    expires_at     TIMESTAMPTZ,
    revoked_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_oauth_access_tokens_user ON oauth_access_tokens(user_id);

CREATE TABLE oauth_refresh_tokens (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    access_token_id BIGINT NOT NULL REFERENCES oauth_access_tokens(id) ON DELETE CASCADE,
    token_hash      TEXT NOT NULL UNIQUE,
    expires_at      TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Browser sessions: token, browser, IP, last activity (BOOTSTRAP "Users").
CREATE TABLE sessions (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id          BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash       TEXT NOT NULL UNIQUE,
    user_agent       TEXT,
    ip               INET,
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_sessions_user ON sessions(user_id);

-- Who last touched an overlay (admin panel/CLI write path, epic 22/27).
ALTER TABLE config_overlays
    ADD COLUMN updated_by BIGINT REFERENCES accounts(id) ON DELETE SET NULL;
