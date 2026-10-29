# Interactions

Social interactions on statuses — favourites, boosts, bookmarks,
quotes, custom emoji reactions and pins — plus the matching
ActivityPub activities.

## Storage

Boosts are `statuses` rows with `reblog_of_id` set (Mastodon
semantics); the boost row's `uri` *is* the Announce activity id.
Quotes use `quote_of_id` (+ `quotes_count`) with a `quote_uri`
fallback for remote targets not materialized yet. Pending targets
are resolved *actively*: ingest (and reads of a still-pending status)
enqueue a `RemoteStatusFetch` job that signed-GETs the URI and
materializes the object through the same pipeline as an inbound
`Create`, honoring `domain_blocks` and the search resolver's
same-origin trust rule; the stored `quote_uri` is then back-filled to
`quote_of_id` and `quotes_count` bumps. The job is deduped per URI by
a Redis marker — repeat ingests and re-reads don't pile up fetches.
`favourites.activity_uri` and
`emoji_reactions.activity_uri` record the sent activity id so
`Undo(*)` rebuilds the inner activity from the `activities` ledger
(with a deterministic rebuild fallback).

## Endpoints

- `POST /api/v1/statuses/{id}/favourite|unfavourite`,
  `GET /api/v1/favourites`, `GET …/favourited_by`
- `POST /api/v1/statuses/{id}/reblog|unreblog`,
  `GET …/reblogged_by` — `public|unlisted` targets only (422
  otherwise); optional `visibility` param (`public|unlisted`,
  accepted in the body or query string).
- `POST /api/v1/statuses/{id}/bookmark|unbookmark`,
  `GET /api/v1/bookmarks` — silent: no AP, no notification.
- `GET /api/v1/statuses/{id}/quotes` — posts quoting the target,
  newest first; each row is filtered through `can_see` for the
  viewer (anonymous viewers see the public subset).
- `POST /api/v1/statuses` with `quote_id` — appends the Pleroma `RE:`
  link to the rendered HTML, stamps all FEP-0449 quote fields on the
  object and sends a FEP-044f `QuoteRequest` to remote quoted authors
  (never blocking on the answer; Yonder's `auto_approve_quotes`
  accepts inbound requests).
- `PUT|POST /api/v1/pleroma/statuses/{id}/reactions/{emoji}`,
  `DELETE` same, `GET …/reactions` — unicode emoji or `:shortcode:`
  resolved through `custom_emojis` (unknown → 422).
- `POST /api/v1/statuses/{id}/pin|unpin` — own non-`direct` statuses,
  capped by `content.max_pinned_posts`; reflected in
  `/ap/users/{u}/collections/featured` (public/unlisted pins only).

List endpoints paginate with `max_id`/`min_id`/`limit` + `Link`
headers (`http::paginate`).

## Inbound

`interactions::inbound` runs in the `on_activity` hook (after
signature verification, before pubby's dispatch):

| Activity | Effect | Disposition |
|---|---|---|
| `Like` / `Announce` | normalized rows + counters | `PassThrough` (pubby keeps its `interactions` bookkeeping + `on_interaction_received` fan-out) |
| `Undo(Like/Announce)` | row removal + counter decrement | `PassThrough` |
| `EmojiReact` / `Undo(EmojiReact)` | `emoji_reactions` rows | `Handled` (pubby ignores the type) |
| `Accept(QuoteRequest)` | `QuoteAuthorization` recorded | `Handled` |

Guards: only *local* statuses are touched; actors blocked by the
target's author drop silently. `Undo` arms inspect the inner type —
`Undo(Follow)` and friends stay with pubby.

## Notification events

Interactions *emit events*, never write `notifications` rows: local
actions enqueue `NotificationFanout` with
`{type, from_account_id, status_id, account_id}` (suppressed for
self-interactions, remote recipients and blocked actors); inbound
Like/Announce/quote Creates flow through pubby's
`on_interaction_received`; `EmojiReact` enqueues the same payload
explicitly. Epic 16 owns the job. Bookmarks emit nothing.

## Entity additions

`reblogged`/`favourited`/`bookmarked`/`pinned` viewer flags, `reblog`
nesting (boost rows render the original, one level), `quote_id` +
`quote: {state, quoted_status}` (Mastodon 4.4), and
`pleroma.emoji_reactions` (`{name, count, me, url}`).
