-- Recompute the denormalized `replies_count`/`quotes_count` columns.
--
-- The counters drifted: remote replies materialized by
-- `posts::remote::upsert` never bumped the parent's `replies_count`,
-- and no tombstone path (local delete, remote Delete, scheduled
-- deletion sweep) decremented either counter — deletes inflated them
-- permanently. The write paths now maintain both columns on
-- insert/update/delete; this backfill repairs existing rows in one
-- pass (tombstoned statuses no longer count as replies or quotes).

UPDATE statuses s
SET replies_count = (
    SELECT count(*) FROM statuses r
    WHERE r.in_reply_to_id = s.id AND NOT r.tombstone
);

UPDATE statuses s
SET quotes_count = (
    SELECT count(*) FROM statuses q
    WHERE q.quote_of_id = s.id AND NOT q.tombstone
);
