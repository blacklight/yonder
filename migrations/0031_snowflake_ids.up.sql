-- 0031_snowflake_ids — replace sequential identity ids with
-- time-sortable snowflake ids on every `BIGINT GENERATED ALWAYS AS
-- IDENTITY` primary key.
--
-- Layout: (epoch-millis << 16) + tail, where tail is the low 16 bits
-- of a shared sequence — Mastodon's `timestamp_id` scheme. Ids stay
-- BIGINT (no type or index churn), are non-enumerable (~65k
-- candidates per millisecond rather than 1), and keep strict
-- insertion order inside a millisecond. Timelines already order by
-- (created_at, id); these ids only tighten that alignment.
CREATE SEQUENCE snowflake_seq;

CREATE OR REPLACE FUNCTION timestamp_id(table_name text)
RETURNS bigint AS $$
BEGIN
    RETURN (((date_part('epoch', clock_timestamp()) * 1000))::bigint << 16)
         + (nextval('snowflake_seq') & 65535);
END;
$$ LANGUAGE plpgsql VOLATILE;

-- Backdated variant for objects materialized late — remote statuses
-- and archive imports pass `published`/`original_created_at`, keeping
-- id order aligned with the chronological `created_at` they carry.
CREATE OR REPLACE FUNCTION timestamp_id(table_name text, at timestamptz)
RETURNS bigint AS $$
BEGIN
    RETURN (((date_part('epoch', at) * 1000))::bigint << 16)
         + (nextval('snowflake_seq') & 65535);
END;
$$ LANGUAGE plpgsql VOLATILE;

ALTER TABLE accounts
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('accounts');
ALTER TABLE users
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('users');
ALTER TABLE account_aliases
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('account_aliases');
ALTER TABLE webauthn_credentials
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('webauthn_credentials');
ALTER TABLE backup_codes
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('backup_codes');
ALTER TABLE oauth_applications
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('oauth_applications');
ALTER TABLE oauth_authorizations
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('oauth_authorizations');
ALTER TABLE oauth_access_tokens
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('oauth_access_tokens');
ALTER TABLE oauth_refresh_tokens
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('oauth_refresh_tokens');
ALTER TABLE sessions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('sessions');
ALTER TABLE statuses
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('statuses');
ALTER TABLE polls
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('polls');
ALTER TABLE poll_options
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('poll_options');
ALTER TABLE poll_votes
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('poll_votes');
ALTER TABLE tags
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('tags');
ALTER TABLE tag_follows
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('tag_follows');
ALTER TABLE media_attachments
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('media_attachments');
ALTER TABLE preview_cards
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('preview_cards');
ALTER TABLE follows
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('follows');
ALTER TABLE follow_requests
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('follow_requests');
ALTER TABLE blocks
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('blocks');
ALTER TABLE mutes
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('mutes');
ALTER TABLE account_domain_blocks
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('account_domain_blocks');
ALTER TABLE subscriptions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('subscriptions');
ALTER TABLE account_notes
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('account_notes');
ALTER TABLE lists
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('lists');
ALTER TABLE favourites
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('favourites');
ALTER TABLE bookmarks
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('bookmarks');
ALTER TABLE emoji_reactions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('emoji_reactions');
ALTER TABLE notifications
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('notifications');
ALTER TABLE interactions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('interactions');
ALTER TABLE activities
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('activities');
ALTER TABLE deliveries
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('deliveries');
ALTER TABLE relays
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('relays');
ALTER TABLE domain_blocks
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('domain_blocks');
ALTER TABLE reports
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('reports');
ALTER TABLE moderation_actions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('moderation_actions');
ALTER TABLE audit_log
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('audit_log');
ALTER TABLE custom_emojis
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('custom_emojis');
ALTER TABLE filters
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('filters');
ALTER TABLE filter_keywords
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('filter_keywords');
ALTER TABLE filter_statuses
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('filter_statuses');
ALTER TABLE invites
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('invites');
ALTER TABLE feed_items
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('feed_items');
ALTER TABLE scheduled_jobs
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('scheduled_jobs');
ALTER TABLE web_push_subscriptions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('web_push_subscriptions');
ALTER TABLE account_deletions
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('account_deletions');
ALTER TABLE archive_exports
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('archive_exports');
ALTER TABLE archive_imports
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('archive_imports');
ALTER TABLE scheduled_statuses
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('scheduled_statuses');
ALTER TABLE status_idempotency_keys
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('status_idempotency_keys');
ALTER TABLE emoji_packs
    ALTER COLUMN id DROP IDENTITY,
    ALTER COLUMN id SET DEFAULT timestamp_id('emoji_packs');
