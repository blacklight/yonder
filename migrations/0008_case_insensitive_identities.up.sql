-- Review 01 follow-up: identity uniqueness must be case-insensitive.
-- WebFinger and mention lookups are case-insensitive in practice, so
-- allowing `Alice` + `alice` to coexist permits impersonation and
-- ambiguous lookup (Mastodon enforces lower(username) uniqueness).
-- Same reasoning for hashtag names and user emails.
--
-- Also adds the missing `mutes(target_account_id)` index (blocks has
-- one; mutes didn't).

DROP INDEX uix_accounts_username_domain;
CREATE UNIQUE INDEX uix_accounts_username_domain
    ON accounts (lower(username), lower(COALESCE(domain, '')));

ALTER TABLE users DROP CONSTRAINT users_email_key;
CREATE UNIQUE INDEX uix_users_email ON users (lower(email));

ALTER TABLE tags DROP CONSTRAINT tags_name_key;
CREATE UNIQUE INDEX uix_tags_name ON tags (lower(name));

CREATE INDEX ix_mutes_target ON mutes(target_account_id);
