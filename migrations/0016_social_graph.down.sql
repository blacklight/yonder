DROP TRIGGER IF EXISTS trg_follows_counters_del ON follows;
DROP TRIGGER IF EXISTS trg_follows_counters_ins ON follows;
DROP FUNCTION IF EXISTS yonder_follows_counters();
DROP INDEX IF EXISTS ix_list_accounts_account;
