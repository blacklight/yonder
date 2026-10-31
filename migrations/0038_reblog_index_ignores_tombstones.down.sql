-- 0038 down — restore the tombstone-inclusive unique index. Under
-- 0038 a pair may hold a live boost plus several tombstones; the old
-- predicate allows at most one row, so dedupe first — a live row
-- wins, otherwise the newest tombstone survives. Dependents drop via
-- the existing ON DELETE CASCADE/SET NULL references.
DELETE FROM statuses s
USING (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY account_id, reblog_of_id
               ORDER BY tombstone, id DESC
           ) AS rn
    FROM statuses
    WHERE reblog_of_id IS NOT NULL
) dup
WHERE s.id = dup.id AND dup.rn > 1;

DROP INDEX uix_statuses_reblog;
CREATE UNIQUE INDEX uix_statuses_reblog ON statuses (account_id, reblog_of_id)
    WHERE reblog_of_id IS NOT NULL;
