DROP TABLE IF EXISTS import_items;

-- States that only exist post-0044 collapse onto 'failed' so the
-- restored CHECK holds; 'paused'/'cancelled' sessions resume as
-- failed imports rather than blocking the revert.
UPDATE archive_imports SET state = 'failed' WHERE state IN ('paused', 'cancelled');

ALTER TABLE archive_imports DROP CONSTRAINT archive_imports_state_check;
ALTER TABLE archive_imports ADD CONSTRAINT archive_imports_state_check
    CHECK (state IN ('pending', 'running', 'done', 'failed'));

-- Rows created through the API source have no uploaded file and
-- cannot be represented pre-0044. Reverting drops them (with their
-- import_items and any dependent publication/observation rows).
DELETE FROM archive_imports WHERE source_type = 'api';
ALTER TABLE archive_imports ALTER COLUMN file_key SET NOT NULL;

ALTER TABLE archive_imports
    DROP COLUMN source_type,
    DROP COLUMN source_actor_uri,
    DROP COLUMN source_origin,
    DROP COLUMN options,
    DROP COLUMN stage,
    DROP COLUMN category_state,
    DROP COLUMN cutoff,
    DROP COLUMN cancel_requested,
    DROP COLUMN next_retry_at,
    DROP COLUMN summary;
