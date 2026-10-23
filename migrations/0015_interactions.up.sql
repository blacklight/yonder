-- Epic 13, migration 0015: social interactions (favourite, boost,
-- bookmark, quote, emoji reactions, pins).
--
-- `favourites.activity_uri`/`emoji_reactions.activity_uri` record the
-- sent Like/EmojiReact activity id: `Undo(*)` rebuilds the inner
-- activity from the stored `activities` row (delivery::publish
-- persists the full JSON). `statuses.quote_uri` holds a remote quote
-- target that isn't materialized locally yet — `quote_of_id` is
-- back-filled when the quoted Create arrives.
--
-- Boost rows are `statuses` rows with `reblog_of_id` set (Mastodon
-- semantics); the partial UNIQUE index makes the boost itself
-- idempotent per (booster, target).

ALTER TABLE favourites ADD COLUMN activity_uri TEXT;
ALTER TABLE emoji_reactions ADD COLUMN activity_uri TEXT;

ALTER TABLE statuses ADD COLUMN quote_uri TEXT;
CREATE INDEX ix_statuses_quote_uri ON statuses (quote_uri)
    WHERE quote_uri IS NOT NULL;

CREATE UNIQUE INDEX uix_statuses_reblog ON statuses (account_id, reblog_of_id)
    WHERE reblog_of_id IS NOT NULL;

-- Account-scoped list endpoints (newest first).
CREATE INDEX ix_favourites_account_created ON favourites (account_id, created_at DESC);
CREATE INDEX ix_bookmarks_account_created ON bookmarks (account_id, created_at DESC);
