-- 0061_calendar_tokens — bearer tokens for the personalized
-- calendar subscription feed (`/users/{u}/calendar/{token}.ics`).
--
-- Only the SHA-256 hash is stored (same convention as
-- `oauth_tokens.token_hash`): a database leak must not expose usable
-- calendar URLs. `revoked_at` implements rotation/revocation;
-- `last_used_at` gives the feed handler cheap usage visibility.

CREATE TABLE calendar_tokens (
    id           BIGSERIAL PRIMARY KEY,
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at   TIMESTAMPTZ
);
CREATE INDEX calendar_tokens_account_idx ON calendar_tokens (account_id)
    WHERE revoked_at IS NULL;
