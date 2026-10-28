-- 0033_statuses_object_type — the AP object `type` for remote
-- statuses (`Note`, `Video`, `Audio`, …). NULL for rows ingested
-- before the column existed and for local posts (always notes);
-- remote ingest writes it, and the read-time media self-heal uses it
-- to bound re-fetches to A/V objects.
ALTER TABLE statuses ADD COLUMN object_type text;
