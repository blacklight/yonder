-- 0055_article_posts — FEP-b2b8 long-form text (Article) support.
--
-- `statuses.title`/`summary` carry the AS2 `name`/`summary` of
-- long-form posts (plain text — the AP layer renders `summary` as
-- HTML); `cover_media_id` designates the representative image emitted
-- as the object's `image` (NULL falls back to the first image
-- attachment). `object_type` (0033) doubles as the local post type:
-- new local rows store 'Note'/'Article'/'Question' explicitly.
-- `accounts.default_post_type` is the per-account composer default
-- ('note'|'article'; NULL = note), mirroring
-- `accounts.default_content_type`.

ALTER TABLE statuses
    ADD COLUMN title TEXT CHECK (char_length(title) <= 500),
    ADD COLUMN summary TEXT CHECK (char_length(summary) <= 2000),
    ADD COLUMN cover_media_id BIGINT
        REFERENCES media_attachments(id) ON DELETE SET NULL;

ALTER TABLE accounts
    ADD COLUMN default_post_type TEXT
        CHECK (default_post_type IN ('note', 'article'));
