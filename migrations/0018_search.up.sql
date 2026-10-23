-- Epic 17, migration 0018: search indexes — trigram matching on
-- accounts (username/display_name) and prefix matching on tag names.
--
-- `pg_trgm` already exists (0001) and statuses content/spoiler
-- trigram indexes landed in 0003. Accounts get the same treatment
-- here; tags get a `lower(name)` text_pattern_ops btree because
-- hashtag search is a *prefix* match (autocomplete), which a btree
-- serves better than a trigram GIN for short names.

CREATE INDEX ix_accounts_username_trgm ON accounts USING gin (username gin_trgm_ops);
CREATE INDEX ix_accounts_display_name_trgm ON accounts USING gin (display_name gin_trgm_ops);
CREATE INDEX ix_tags_name_lower_prefix ON tags (lower(name) text_pattern_ops);

-- Backfill `content` for remote statuses materialized before the
-- HTML→text fallback existed: no `source` meant `content` stayed '',
-- making them unsearchable. `content_html` (sanitized) is the only
-- text they carry; strip tags and collapse whitespace. Best-effort —
-- HTML entities like &amp; are left literal. Tombstoned rows
-- (content_html = '') are skipped. The down migration can't
-- distinguish backfilled text from a real `source`, so it doesn't
-- revert this.
UPDATE statuses
SET content = btrim(regexp_replace(content_html, '<[^>]+>|\s+', ' ', 'g'))
WHERE content = ''
  AND content_html IS NOT NULL
  AND content_html <> '';
