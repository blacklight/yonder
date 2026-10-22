-- Epic 09 (user profile editing): blurhash placeholders for locally
-- stored avatar/header media. The files themselves live under
-- `media.local_path` keyed by `avatar_path`/`banner_path`.
ALTER TABLE accounts
    ADD COLUMN avatar_blurhash TEXT,
    ADD COLUMN header_blurhash TEXT;
