-- Epic 14, migration 0016: social graph — list-membership lookup index
-- and `follows` counter triggers.
--
-- `followers_count`/`following_count` are owned by the triggers below
-- for *local* accounts (every `follows` writer — the REST path, the
-- pubby storage adapter, imports, moves and cascaded deletes — then
-- stays honest with no app-level bookkeeping). Remote accounts' counts
-- come from their actor documents (`followersCount` et al. mapped in
-- `upsert_remote_account`), never from triggers.
--
-- The backfill clears drift already accumulated by the inbound paths
-- (`store_follower`/`remove_follower` never touched the counters).

-- "Which lists contain this account" lookups
-- (`GET /api/v1/accounts/{id}/lists`, import cleanup).
CREATE INDEX ix_list_accounts_account ON list_accounts (account_id);

CREATE OR REPLACE FUNCTION yonder_follows_counters() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE accounts SET following_count = following_count + 1
        WHERE id = NEW.account_id AND domain IS NULL;
        UPDATE accounts SET followers_count = followers_count + 1
        WHERE id = NEW.target_account_id AND domain IS NULL;
        RETURN NEW;
    ELSIF TG_OP = 'DELETE' THEN
        UPDATE accounts SET following_count = GREATEST(0, following_count - 1)
        WHERE id = OLD.account_id AND domain IS NULL;
        UPDATE accounts SET followers_count = GREATEST(0, followers_count - 1)
        WHERE id = OLD.target_account_id AND domain IS NULL;
        RETURN OLD;
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER trg_follows_counters_ins
    AFTER INSERT ON follows
    FOR EACH ROW EXECUTE FUNCTION yonder_follows_counters();

CREATE TRIGGER trg_follows_counters_del
    AFTER DELETE ON follows
    FOR EACH ROW EXECUTE FUNCTION yonder_follows_counters();

-- One-shot recount: drop the drift accumulated before the triggers.
UPDATE accounts a SET
    following_count = (SELECT count(*) FROM follows f WHERE f.account_id = a.id),
    followers_count = (SELECT count(*) FROM follows f WHERE f.target_account_id = a.id)
WHERE a.domain IS NULL;
