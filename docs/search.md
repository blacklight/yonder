# Search

Mastodon-compatible search over accounts, statuses and hashtags —
trigram matching by default, remote FQN/URL lookup with caching, and
the social-graph/discoverability filters applied everywhere.

## Endpoints

| Route | Auth | Notes |
|---|---|---|
| `GET /api/v2/search` | optional | v2 shapes (Tag objects) |
| `GET /api/v1/search` | optional | v1 shapes (hashtags as strings) |

Parameters: `q` (required), `type` (`accounts` / `statuses` /
`hashtags`), `resolve`, `following`, `account_id`, `offset`, `limit`
(default 20, max 40), `exclude_unreviewed` (accepted, no-op).
Pagination is **offset-based** — no `Link` headers.

- Anonymous search is allowed when `search.unauthenticated_enabled`
  (default `true`); authenticated callers need the `read:search` scope
  (base `read` covers it).
- Rate-limited under the `search` bucket.
- **Autocomplete** is the same endpoint — the frontend calls
  `?type=accounts|hashtags&limit=5&resolve=false`; account and tag
  matching runs in prefix mode when `type` is set and `q` is a plain
  word.

## Query classification (`q`)

| Shape | Kind | Behavior |
|---|---|---|
| `https://…` | URL | Local/known-row lookup, then remote fetch per `resolve` |
| `@user@domain` / `user@domain` | FQN | Exact account match, then WebFinger resolve per `resolve` |
| `#tag` | Hashtag | Exact tag match (prefix in autocomplete mode) |
| anything else | Text | Substring match across all three buckets |

## Remote lookup and caching

Remote resolution requires the full gate chain: **authenticated
caller + `resolve=true` + `search.remote_fetch` + federation up**
(`state.ap`). Mastodon ignores `resolve` for anonymous callers, and so
does Yonder.

- **FQN** (`@user@domain`): the cached `accounts` row wins when
  present; otherwise WebFinger via pubby's SSRF-guarded client, a bare
  `accounts` stub upsert (shared with mention resolution in
  `activitypub::lookup`), and a `RemoteActorFetch` job to enrich it.
  Pubby answers `https://{domain}/@{username}` when WebFinger fails —
  that fabricated guess is treated as a miss: no stub row, and the
  failure is negatively cached.
- **URL** (`https://…`): local `@{user}`/`@{user}/{id}` and
  `/ap/users/…` path shapes resolve directly (a local miss never
  triggers a remote fetch); cached remote rows match `statuses.uri`/
  `url` or `accounts.actor_id`/`url`; unknown remote URLs take a
  **signed GET** (instance-actor key material, so `authorized_fetch`
  servers answer). The fetch follows validated redirects, and trust
  is evaluated against the URL that *served* the document — the last
  redirect hop — so domain-split servers that redirect web permalinks
  to their canonical AP domain (e.g. a blog domain 302ing to the
  fediverse domain) resolve on first lookup. A fetched document is
  only trusted when it provably belongs to the host it came from:
  actor documents must have `id == <the serving URL>`; notes must
  have `id` on the serving origin and an `attributedTo` on that same
  origin. A redirect landing back on this instance resolves locally
  instead of materializing our own objects as remote. Trusted actors
  go through
  `upsert_remote_actor_row`; trusted objects (the same set the
  inbound pipeline materializes: `Note`/`Question`/`Article`/`Page`
  plus `Audio`/`Video`/`Event`) are wrapped in a synthetic `Create`
  and run through the inbound `posts::remote` pipeline — the instance
  "learning" a post behaves exactly like receiving the `Create` in
  the inbox. When the fetched document is itself an *activity* (some
  servers — Songhive/Funkwhale `/activities/…` URLs — serve a
  `Create`/`Listen` rather than the bare object), an inline `object`
  is descended one level under the same trust rules.
- The signed fetch is triggered by an authenticated query, so the
  instance actor's signature is disclosed to any host a user names;
  targets are bounded by the federation client's SSRF guard
  (`federation.allow_private_targets` must stay off in production)
  and the `search` rate-limit bucket.
- **Negative lookups** (failed fetches, unresolvable FQNs, non-entity
  or untrusted documents) are cached in Redis
  (`search:neglookup:*`) for 10 minutes.
- Resolved hits bypass the discoverability filters (an exact FQN/URL
  is an explicit ask) but never the social-graph exclusions — a
  blocked/muted/domain-blocked or suspended actor is filtered even
  from an exact URL lookup, and the resolved status still passes the
  caller visibility check before it's returned. Lookups are
  single-hit: they answer page one only (`offset > 0` → empty).

## Filtering

Every query composes:

- **Liveness**: `accounts.suspended_at`/`deleted_at` hidden
  everywhere; `domain_blocks` with `action='suspend'` hides remote
  actors everywhere and `limit`/`remove_from_timeline` hides their
  statuses (public-scope semantics, same as timelines).
- **Social graph** (authenticated viewers): blocks in either
  direction, unexpired mutes, per-user `account_domain_blocks`.
- **Discoverability**: `accounts.discoverable = false` and
  `users.indexable = false` hide accounts from text search — except
  accounts the caller follows and exact FQN/URL matches.
- **Author opt-out**: `users.include_public_posts_in_search = false`
  hides that author's statuses from everyone but the author themself.
- **Visibility**: anonymous callers see `public`/`unlisted` only;
  viewers additionally see `followers_only`/`private` from authors
  they follow and `direct` posts that mention them. Reblogs are
  excluded from text matching (the original is searchable itself).

## Matching details

- **Accounts**: `ILIKE` over `username`, `display_name` **and**
  `domain` — a domain substring search surfaces a remote instance's
  known actors. Ranking: exact username first, locals before remotes,
  then `similarity()` over username + display name. A leading `@` on a
  text query is stripped (Mastodon's behavior).
- **Statuses**: `ILIKE '%q%'` over `content` and `spoiler_text`
  (trigram indexes from migration 0003). `content` holds plain text
  for every row — for remote statuses it's `source.content`, or the
  object's HTML converted to text when no `source` is present.
  `account_id` scopes the search to one author (404 for a missing
  account).
- **Hashtags**: `lower(tags.name)` — exact for text queries, prefix
  (`text_pattern_ops` index, migration 0018) in autocomplete mode.
  Tags only surface when the caller can see ≥1 carrying status.

## Index backend

`search::index::SearchIndex` is the pluggable seam
(`search_accounts`/`search_statuses`/`index_*`/`remove_*`).
`PostgresIndex` delegates to the trigram queries and is the default
and only shipping backend — the DB *is* the index, so `index_*`/
`remove_*` are no-ops. Setting `search.elasticsearch_url` logs a
once-per-process warning and falls back to Postgres until an ES/OS
implementation lands.

## Response shape

`{ "accounts": [], "statuses": [], "hashtags": [] }` — Account and
Status are the standard entities; v2 hashtags are Tag objects with an
empty `history` (trending is epic 20) plus a `yonder.statuses_count`
extension carrying the lifetime count of statuses the caller can see.
