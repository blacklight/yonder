-- Epic 16, migration 0017: notification dedupe, the `admin.report`
-- type, and report linkage.
--
-- `ix_notifications_dedupe` makes generation idempotent: the
-- `NotificationFanout` job runs at-least-once and inbound AP
-- activities can be redelivered. `COALESCE(status_id, 0)` is required
-- because NULLs are distinct in unique indexes and relationship
-- notifications (follow, follow_request) carry no status.

CREATE UNIQUE INDEX ix_notifications_dedupe
    ON notifications (account_id, from_account_id, type, COALESCE(status_id, 0));

ALTER TABLE notifications
    ADD COLUMN report_id BIGINT REFERENCES reports(id) ON DELETE SET NULL;

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning',
        'admin.report'));
