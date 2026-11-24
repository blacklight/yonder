-- Epic 32 (events), migration 0062: FEP-8a8e notification types and
-- `event` as a composer default post type.
--
-- The four `pleroma:*` types carry Nicolex/Pleroma wire names —
-- `pleroma:participation_request`, `pleroma:participation_accepted`,
-- `pleroma:event_update`, `pleroma:event_reminder`.

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning',
        'admin.report', 'move',
        'pleroma:participation_request', 'pleroma:participation_accepted',
        'pleroma:event_update', 'pleroma:event_reminder'));

ALTER TABLE accounts DROP CONSTRAINT accounts_default_post_type_check;
ALTER TABLE accounts ADD CONSTRAINT accounts_default_post_type_check
    CHECK (default_post_type IN ('note', 'article', 'event'));

-- Reminder-sweep idempotency marker (Section 12) is `event_participations.reminded_at`,
-- already created by 0057_events — do not re-add it here.
