-- Epic 16, migration 0017 (down): restore the epic-02 notification
-- schema.

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning'));

ALTER TABLE notifications DROP COLUMN IF EXISTS report_id;

DROP INDEX IF EXISTS ix_notifications_dedupe;
