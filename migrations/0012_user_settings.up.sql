-- Epic 10 (user settings): columns that had no storage yet.
--
-- `users.block_stranger_notifications` — BOOTSTRAP "block notifications
-- from strangers" preference (epic 16 applies it).
--
-- `accounts.default_sensitive` — Mastodon `source[sensitive]` posting
-- default ("mark new statuses sensitive by default"), surfaced as
-- `source.sensitive` / `posting:default:sensitive`.
--
-- NULL semantics stay meaningful elsewhere: `users.locale`/`theme`/
-- `accent`/`scheduled_deletion_days` and `accounts.default_language` keep
-- NULL = unset (system/app-language fallback, 0 deletion days), so no
-- backfill or CHECK is added here — the API layer validates.

ALTER TABLE users
    ADD COLUMN block_stranger_notifications BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE accounts
    ADD COLUMN default_sensitive BOOLEAN NOT NULL DEFAULT false;
