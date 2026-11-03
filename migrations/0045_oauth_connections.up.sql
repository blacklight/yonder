-- Migration 0045: remote OAuth connections for the API import source.
--
-- Holds the destination-side record of an authorization the user
-- granted on a remote instance. Token and client-secret material is
-- stored AEAD-encrypted at rest (bytea) — the application-layer key
-- domain is derived from the existing secret-key design; plaintext
-- never reaches job payloads, URLs, logs, or API responses.

CREATE TABLE oauth_connections (
    id                BIGINT PRIMARY KEY DEFAULT timestamp_id('oauth_connections'),
    user_id           BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Exact API origin, e.g. https://remote.example — SSRF-constrained.
    origin            TEXT NOT NULL,
    -- Canonical actor URI verified via the remote verify_credentials.
    source_actor_uri  TEXT NOT NULL,
    source_acct       TEXT,
    scopes            TEXT NOT NULL DEFAULT '',
    access_token_enc  BYTEA NOT NULL,
    refresh_token_enc BYTEA,
    client_id_enc     BYTEA,
    client_secret_enc BYTEA,
    token_expires_at  TIMESTAMPTZ,
    -- 'pending' = authorization in flight, 'active' = usable,
    -- 'revoked'/'expired' = terminal.
    state             TEXT NOT NULL DEFAULT 'pending'
                      CHECK (state IN ('pending', 'active', 'revoked', 'expired')),
    remote_app_uri    TEXT,
    expires_at        TIMESTAMPTZ,
    revoked_at        TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, origin, source_actor_uri)
);

CREATE INDEX ix_oauth_connections_owner ON oauth_connections (user_id, state);
