DROP TABLE IF EXISTS archive_imports;
DROP TABLE IF EXISTS archive_exports;
DROP TABLE IF EXISTS account_deletions;
DROP INDEX IF EXISTS uix_statuses_account_original;
ALTER TABLE users DROP COLUMN IF EXISTS invite_id;
ALTER TABLE accounts DROP COLUMN IF EXISTS deleted_at, DROP COLUMN IF EXISTS moved_at;
