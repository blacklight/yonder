-- Epic 07, migration 0010: user-managed token names + session↔token link.
--
-- `oauth_access_tokens.name` labels user-managed API tokens
-- (POST /api/v1/tokens); NULL for grant-issued tokens.
--
-- `sessions.access_token_id` links an interactive login's session row to
-- the access token it wraps: revoking the session kills the token, and
-- `token_hash` (pre-existing, NOT NULL) holds the same sha256-hex hash so
-- session rows stay resolvable on their own.

ALTER TABLE oauth_access_tokens
    ADD COLUMN name TEXT;

ALTER TABLE sessions
    ADD COLUMN access_token_id BIGINT REFERENCES oauth_access_tokens(id) ON DELETE CASCADE;

-- Authorized-app revocation scans tokens of a (user, application) pair.
CREATE INDEX ix_oauth_access_tokens_app_user
    ON oauth_access_tokens(application_id, user_id);
