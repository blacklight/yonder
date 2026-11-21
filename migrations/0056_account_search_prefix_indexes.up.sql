-- 0056_account_search_prefix_indexes — autocomplete prefix indexes.
--
-- Account autocomplete (`type=accounts`) matches `lower(username)` and
-- `lower(display_name)` with `LIKE 'q%'`. Trigram GIN indexes don't
-- help very short prefixes, so btree `text_pattern_ops` indexes back
-- the predicate. Substring search keeps using the trigram indexes
-- from 0018; generic account search no longer substring-matches
-- `domain` (a domain-shaped `q` exact-matches `accounts.domain`
-- instead), because domain substring matching hit hundreds of
-- thousands of cached remote actors at fediverse scale.
--
-- `IF NOT EXISTS` lets large deployments pre-create the indexes with
-- `CREATE INDEX CONCURRENTLY` (same names) before running migrations,
-- avoiding a write-blocking build on `accounts`.

CREATE INDEX IF NOT EXISTS ix_accounts_username_lower_prefix
    ON accounts (lower(username) text_pattern_ops);

CREATE INDEX IF NOT EXISTS ix_accounts_display_name_lower_prefix
    ON accounts (lower(display_name) text_pattern_ops);
