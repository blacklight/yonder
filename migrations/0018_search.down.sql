-- Epic 17, migration 0018 rollback: drop the search indexes.

DROP INDEX IF EXISTS ix_accounts_username_trgm;
DROP INDEX IF EXISTS ix_accounts_display_name_trgm;
DROP INDEX IF EXISTS ix_tags_name_lower_prefix;
