-- 0055_article_posts down

ALTER TABLE accounts DROP COLUMN default_post_type;

ALTER TABLE statuses
    DROP COLUMN cover_media_id,
    DROP COLUMN summary,
    DROP COLUMN title;
