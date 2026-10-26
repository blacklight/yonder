-- `statuses_count` was never maintained by the create/tombstone paths
-- — every account (local and remote) reported "0 posts" unless a
-- remote `statusesCount`/`outbox totalItems` happened to land. Rebuild
-- it from indexed statuses. `GREATEST` keeps remote-reported values
-- (an outbox `totalItems` larger than our local index).
UPDATE accounts a
   SET statuses_count = GREATEST(
       a.statuses_count,
       (SELECT count(*) FROM statuses s
         WHERE s.account_id = a.id AND NOT s.tombstone)
   );
