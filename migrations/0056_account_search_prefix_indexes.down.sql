-- 0056_account_search_prefix_indexes down

DROP INDEX IF EXISTS ix_accounts_display_name_lower_prefix;
DROP INDEX IF EXISTS ix_accounts_username_lower_prefix;
