# Content filters

Mastodon-compatible **server-side** filters (`/api/v2/filters` — the
deprecated v1 surface is not implemented). A filter pairs a title and
an action with keywords and/or explicitly pinned statuses, and applies
in one or more *contexts*.

## API

| Route | Scope | Notes |
|---|---|---|
| `GET /api/v2/filters` | `read:filters` | caller's filters, newest-first |
| `POST /api/v2/filters` | `write:filters` | `title`, `context[]`, `filter_action`, `expires_in`, `keywords_attributes[]` |
| `GET/PUT/DELETE /api/v2/filters/{id}` | read/write | owner-scoped — foreign ids 404 |
| `GET/POST /api/v2/filters/{id}/keywords` | read/write | `{keyword, whole_word}` |
| `GET/PUT/DELETE /api/v2/filters/keywords/{id}` | read/write | owner-scoped via the filter |
| `GET/POST /api/v2/filters/{id}/statuses` | read/write | pin a specific status |
| `DELETE /api/v2/filters/statuses/{id}` | `write:filters` | unpin (id is the pin row) |

`context[]` ⊆ `home notifications public thread account`;
`filter_action` ∈ `warn hide blur` (Mastodon's action names).
`expires_in` is seconds-from-now; `0`/`null` clears it on update
(`0` is rejected on create — a born-expired filter is a client bug).
Per-filter keywords are capped at `content.max_filter_keywords`
(default 100), filters per account at `content.max_filters`
(default 100, a soft cap). Expired filters stay visible to CRUD but
are never applied.

## Matching

- **Word keywords** match `content` + `spoiler_text` case-insensitively.
  `whole_word: true` compiles to Postgres `\m…\M` POSIX boundaries with
  every regex metacharacter escaped (literal match only).
- **`#hashtag` keywords** match the status's `status_tags` rows
  (the normalized tag graph), not literal `#` text — so a post that
  mentions the tag in text but isn't tagged still shows.
- **Pinned statuses** (`filter_statuses`) match by membership.

## Application

`hide` rules compile to a single `NOT (m1 OR m2 …)` clause spliced
into `timelines::filter::build_where` (`src/filters/apply.rs`), so
paging stays correct. Context mapping:

| Context | Applies to |
|---|---|
| `home` | home + list timelines |
| `public` | public/local/federated/tag timelines, `/api/{v1,v2}/search` status results |
| `account` | `GET /api/v1/accounts/{id}/statuses` |
| `notifications` | `GET /api/v1/notifications` list + `/{id}`, the live WebSocket push, and the unread-count badge |

`warn`/`blur` rules keep the row but attach Mastodon's `filtered`
attribute (`[{filter, keyword_matches}]`) to the Status entity —
attached at the endpoint layer (`filters::apply::attach_filtered`),
which knows the context; `posts::entity` does not.

Home fan-out stays filter-free: the Redis home index is an
opportunistic cache and SQL revalidation (`timelines::fanout::read_home`)
is the authoritative filter point.

## Followed hashtags in home

`POST /api/v1/tags/{name}/follow` (name-keyed, like Mastodon) both
extends `home_clauses` (`EXISTS status_tags ⋈ tag_follows`, public
posts only) and fans the post out to tag followers' Redis home
indexes, bounded by `timelines.fanout_batch_size`. Readers beyond
that batch still see the post via the SQL top-up.

## Trending hashtags

`GET /api/v1/trends/tags[?period=day|week|month][&limit][&offset]` —
the `period` selector is a Yonder extension (Mastodon's route has
none; default `week`). Ranking counts status uses over the window
with `COUNT(DISTINCT account_id)` ordering uses-desc/accounts-desc,
floored at `timelines.trends_min_accounts` (default 2) so a single
account can't own the list. Results are Redis-cached
(`timelines.trends_cache_seconds`, default 300; `0` disables) under
`trends:tags:{period}:{all|local}`; `timelines.trends_include_remote`
(default true) toggles remote-author inclusion. The cached ranking is
capped at 1000 rows — `offset` pages inside it, so offsets past the
ceiling return an empty page. Counts only consider anonymous-visible
statuses: `public`, non-tombstone, non-boost, excluding suspended and
limited/removed domains. `history[]` is the trailing 7 days,
zero-filled.
