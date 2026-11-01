-- Per-user default post content type (text/plain or text/markdown).
--
-- NULL = follow the instance default (`content.default_post_content_type`),
-- matching the NULL semantics of `accounts.default_language`. The API
-- layer validates writes; the CHECK is a backstop.

ALTER TABLE accounts
    ADD COLUMN default_content_type TEXT
    CHECK (default_content_type IN ('text/plain', 'text/markdown'));
