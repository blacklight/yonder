ALTER TABLE accounts DROP CONSTRAINT accounts_default_post_type_check;
ALTER TABLE accounts ADD CONSTRAINT accounts_default_post_type_check
    CHECK (default_post_type IN ('note', 'article'));

ALTER TABLE notifications DROP CONSTRAINT notifications_type_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_type_check
    CHECK (type IN (
        'follow', 'follow_request', 'mention', 'favourite',
        'reblog', 'quote', 'poll', 'status', 'update',
        'emoji_reaction', 'admin.sign_up', 'moderation_warning',
        'admin.report', 'move'));

ALTER TABLE event_participations DROP COLUMN reminded_at;
