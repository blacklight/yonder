# Timelines

Mastodon-compatible timeline reads — home, public (local/federated),
list and tag — plus the Redis-assisted home fan-out and streaming
hooks.

## Endpoints

| Route | Auth | Scope |
|---|---|---|
| `GET /api/v1/timelines/home` | required | own + followed accounts |
| `GET /api/v1/timelines/public` | optional | `?local=true` local only; `?remote=true` remote only |
| `GET /api/v1/timelines/list/{id}` | required | members of the caller's list |
| `GET /api/v1/timelines/tag/{hashtag}` | optional | public statuses carrying the tag (`?local=true` supported) |

All four accept `max_id` / `min_id` / `since_id` / `limit` (default 20,
max 40) and `only_media`; all emit Mastodon `Link` headers
(`rel="next"` = `max_id`, `rel="prev"` = `min_id`) and are rate-limited
under the `timeline` bucket.

## Followed tags & content filters (epic 20)

- **Followed hashtags** (`POST /api/v1/tags/{name}/follow`): home
  admits `public` statuses tagged with a followed tag via an `EXISTS
  status_tags ⋈ tag_follows` clause in `home_clauses`, and fan-out
  ZADDs tagged posts into tag followers' home indexes (bounded by
  `fanout_batch_size`; the SQL top-up covers the rest).
- **Content filters**: `Filters::hide` carries the viewer's compiled
  `hide` predicate (`filters::apply::hide_clause`) for the context in
  play — `Home` for home/list, `Public` for public/tag, `Account` for
  `accounts/{id}/statuses`. `warn`/`blur` rules surface as the
  `filtered` attribute via `filters::apply::attach_filtered`.
- **Trends**: `GET /api/v1/trends/tags?period=day|week|month`
  (`tags::trends`) counts anonymous-visible tag usage in the window,
  floored at `timelines.trends_min_accounts` and cached for
  `timelines.trends_cache_seconds`.

## Read path

One predicate builder (`src/timelines/filter.rs`) shapes every scope.
`statuses` aliased `s`, author `accounts` aliased `a`, viewer bound as
`$1` when present — the convention `social::visibility`'s exclusion
fragments already use.

- **Visibility.** Public scopes (`local`, `federated`, `tag`) serve
  `public` only — `unlisted` never appears, matching Mastodon's public
  streams. Home admits `public`, `unlisted`, `followers_only`/`private`
  (the follows join doubles as the gate) and `direct` when the viewer
  is mentioned. List admits all non-`direct` visibilities from members.
- **Replies.** Home shows replies addressed to the viewer, to the
  author itself, or to someone the viewer follows. List replies follow
  `lists.replies_policy` (`followed` / `list` / `none`). Public and tag
  timelines include replies — matching Mastodon, whose public
  timelines do not filter them.
- **Moderation.** Blocks (either direction), unexpired mutes and
  per-user domain blocks hide the author for that viewer.
  `accounts.suspended_at`/`deleted_at` and instance `domain_blocks`
  with `action = 'suspend'` hide the author everywhere;
  `limit`/`remove_from_timeline` additionally hide remote authors from
  the public scopes only.
- **Languages.** `users.filter_languages` hides matching-status
  languages for authenticated viewers (`NULL` language passes).
- **Boosts.** Boost rows (`reblog_of_id`) appear in every scope their
  audience rules allow; home additionally hides a followee's boosts
  when `follows.show_reblogs` is off.
- **Exclusive lists.** `lists.exclusive = true` removes member posts
  from the owner's home (the list timeline still shows them).

Everything above is evaluated at read time — mutes, hides and settings
changes take effect immediately, with no re-fan-out.

## Ordering

Every timeline scope orders by `(created_at, id)` — `created_at` keeps
the remote `published` time on federated statuses (clamped to
`now()`), so an object fetched long after publication sorts by its
real date rather than surfacing on top because it was ingested last.
`max_id`/`since_id` stay status ids on the wire; reads translate them
to the cursor row's `(created_at, id)` position.

## Home fan-out

Each home feed is a Redis sorted set
`{prefix}:timeline:home:v2:{account_id}` (member = `statuses.id`,
score = `created_at` epoch microseconds — the `v2` key replaced
id-scored indexes so score domains never mix), maintained by
`TimelineFanOut` jobs:

- `{ "status_id": N }` — `ZADD` into every local follower's index
  (author included), then `ZREMRANGEBYRANK` trims to
  `timelines.home_max_entries` (800). Enqueued on publish: local
  `POST /statuses`, local boosts, remote `Create`, remote `Announce`.
  `direct` and tombstoned rows are skipped.
- `{ "backfill_for": F, "of": T }` — seed `F`'s index with `T`'s recent
  non-`direct` statuses (bounded by the cap, `show_reblogs`-aware).
  Enqueued when a follow lands — immediately for local targets,
  delayed ~2s off the inbound `Accept` for remote ones (pubby's
  dispatch must write the `follows` row first).

Reads (`timelines::fanout::read_home`) take a `ZREVRANGEBYSCORE`
window (`ZRANGEBYSCORE` for `min_id`), **re-validate every id against
the SQL predicate**, and top up from `query::fetch` whenever the page
comes back short — evicted or never-fanned entries can't produce
gaps. A missing key or any Redis error falls back to the identical
SQL page; a *missing* key also repopulates the index with the newest
`rebuild_page_size` unfiltered ids (trimmed to `home_max_entries`) so
the next read hits it. An existing key with an empty window — a
sparse page or a feed's end — is left alone. Postgres is the source
of truth throughout; the index is opportunistic, not a completeness
proof — entries whose fan-out never ran are absent until the next
rebuild.

Deletions (`tombstone`, remote `Delete`, `unreblog`, `Undo(Announce)`)
`ZREM` the id best-effort; the tombstone predicate is the guarantee.

## Streaming

`update`/`delete` events publish on the `EventBus` at the write site
(`timelines::stream`). Audience follows visibility:

- `public` statuses → `public`, `public:local` when local,
  `hashtag:{tag}` / `hashtag:local:{tag}` for each tag, `user:{uid}`
  for the author and every local follower (`uid` is `users.id`), and
  `list:{list_id}:{uid}` for local lists containing the author.
- `unlisted`/`followers_only` → `user:` and `list:` streams only
  (followers and list owners — the same accounts that can read them).
- `direct` → `user:{uid}` for the author and mentioned local users
  only; never public, hashtag or list streams.

Fan-out and `update` events fire only when an object is *new* — a
redelivered remote `Create` re-runs the idempotent upsert but does not
re-notify. Remote materialization publishes through `ApCore::bus`,
which is the real bus in the HTTP process and a detached no-op
elsewhere — cross-process fan-out is a later concern.

## Config (`[timelines]`)

| Key | Default | Meaning |
|---|---|---|
| `home_max_entries` | 800 | ZSET trim size per home feed |
| `fanout_batch_size` | 500 | followers per fan-out batch |
| `rebuild_page_size` | 400 | ids pulled when repopulating a cold index |

## Known gaps

- Status **edits** don't publish `status.update` stream events.
- Archive imports don't fan out (their bulk inserts would flood the
  index).
- A **standalone worker** process carries a detached `EventBus`, so
  remote-status `update`/`delete` events produced by pubby dispatch
  there never reach WebSocket clients (the combined `serve` process
  shares the real bus).
- `min_id` returns the `limit` statuses immediately newer than the
  cursor, ordered newest-first — the Mastodon contract.
