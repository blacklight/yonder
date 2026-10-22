-- Revert epic 12, migration 0014.

DROP TABLE status_idempotency_keys;
DROP TABLE scheduled_status_media;
DROP TABLE scheduled_statuses;

ALTER TABLE media_attachments
    DROP CONSTRAINT media_attachments_focus_x_range,
    DROP CONSTRAINT media_attachments_focus_y_range,
    DROP COLUMN focus_x,
    DROP COLUMN focus_y;

DROP INDEX ix_statuses_scheduled_deletion;
ALTER TABLE statuses DROP COLUMN scheduled_deletion_at;
ALTER TABLE statuses DROP COLUMN content_html;
