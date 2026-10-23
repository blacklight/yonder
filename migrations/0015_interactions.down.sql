DROP INDEX IF EXISTS ix_bookmarks_account_created;
DROP INDEX IF EXISTS ix_favourites_account_created;
DROP INDEX IF EXISTS uix_statuses_reblog;
DROP INDEX IF EXISTS ix_statuses_quote_uri;
ALTER TABLE statuses DROP COLUMN IF EXISTS quote_uri;
ALTER TABLE emoji_reactions DROP COLUMN IF EXISTS activity_uri;
ALTER TABLE favourites DROP COLUMN IF EXISTS activity_uri;
