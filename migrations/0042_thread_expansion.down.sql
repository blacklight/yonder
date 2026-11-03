DROP TABLE IF EXISTS status_expansions;

ALTER TABLE statuses
    DROP COLUMN remote_likes_count,
    DROP COLUMN remote_shares_count,
    DROP COLUMN remote_quotes_count,
    DROP COLUMN remote_counts_fetched_at,
    DROP COLUMN last_selected_at,
    DROP COLUMN fetched_at;

DROP INDEX IF EXISTS ix_statuses_in_reply_to_uri;
ALTER TABLE statuses DROP COLUMN in_reply_to_uri;
