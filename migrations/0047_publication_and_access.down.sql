ALTER TABLE sessions DROP COLUMN access_class;
ALTER TABLE oauth_access_tokens DROP COLUMN access_class;

DROP INDEX IF EXISTS ix_notifications_dedupe;
CREATE UNIQUE INDEX ix_notifications_dedupe
    ON notifications (account_id, from_account_id, type, COALESCE(status_id, 0));

ALTER TABLE notifications
    DROP COLUMN migration_event_id,
    DROP COLUMN target_account_id;

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning',
        'admin.report'));

DROP TABLE IF EXISTS publication_work;
