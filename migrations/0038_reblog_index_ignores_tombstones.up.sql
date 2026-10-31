-- 0038: a tombstoned boost must not block re-boosting the same
-- target. `uix_statuses_reblog` counted tombstones, so after an
-- unreblog the `INSERT ... ON CONFLICT DO NOTHING` always lost to
-- the dead row while every read filters `NOT tombstone` — the API
-- reblog fell through to a `fetch_one` that found no live row (500
-- "no rows returned"), and an inbound re-Announce was silently
-- dropped. The predicate now matches the reads: at most one live
-- boost per (booster, target); each boost cycle gets a fresh row —
-- fresh snowflake id (correct timeline position) and fresh
-- `uri` (a reused Announce id would confuse remote servers that
-- already saw the Undo).
DROP INDEX uix_statuses_reblog;
CREATE UNIQUE INDEX uix_statuses_reblog ON statuses (account_id, reblog_of_id)
    WHERE reblog_of_id IS NOT NULL AND NOT tombstone;
