# Posting

Mastodon-compatible status creation, rendering, media, polls,
scheduling, link previews and ActivityPub federation.

## Endpoints

| Method | Path | Scope | Notes |
| ------ | ---- | ----- | ----- |
| POST | `/api/v1/statuses` | `write:statuses` | create; `Idempotency-Key` replays |
| GET | `/api/v1/statuses/{id}` | — | `can_see` gated |
| GET | `/api/v1/statuses/{id}/context` | — | ancestors + descendants, `can_see` gated |
| PUT | `/api/v1/statuses/{id}` | `write:statuses` | owner edit → `Update` |
| DELETE | `/api/v1/statuses/{id}` | `write:statuses` | owner → tombstone + `Delete` |
| POST | `/api/v1/media` | `write:media` | multipart `file`/`description`/`focus` |
| PUT | `/api/v1/media/{id}` | `write:media` | alt text + focus |
| POST | `/api/v2/media` | `write:media` | same upload shape |
| GET | `/api/v2/media/{id}` | `read:statuses` | processing status |
| GET/POST | `/api/v1/polls/{id}`/`{id}/votes` | `read`/`write:statuses` | results + atomic voting |
| GET/PUT/DELETE | `/api/v1/scheduled_statuses[/…]` | `read`/`write:statuses` | list/show/reschedule/cancel |

## Rendering

- `text/plain`: pubby `render_post_html` (escape → link → hashtag →
  `<br>`) plus a mention-anchor pass.
- `text/markdown`: `pulldown-cmark` → `ammonia` allowlist (no
  `script`/`style`/`iframe`/`img`; `a[href]` restricted to
  `content.valid_url_schemes`) → text-node enhancement that linkifies
  bare URLs/`#tags`/`@mentions`, skipping `code`/`pre`/`a` subtrees.
- CW/`spoiler_text` is always escaped plain text.
- Rendered HTML is stored in `statuses.content_html`; inbound remote
  HTML is re-sanitized through the same allowlist.

## Visibility

Stored: `public`, `unlisted`, `followers_only`, `direct` (input
`private` normalizes to `followers_only`; wire output maps back to
`private`). `can_see` enforces author/followers/mention/public
semantics; the same predicate guards the poll endpoints (a poll is
only as visible as its status) and the AP object GET — private
objects are never served from the shared doc-cache, and unsigned or
unauthorized fetches of them get a 404, never a revealing 401.

## Idempotency

`Idempotency-Key` is honored atomically through
`status_idempotency_keys` (`UNIQUE (account_id, key)`): a fresh insert
wins the reservation, a completed record with the same fingerprint
replays its typed target (`status`/`scheduled_status`), a different
payload is a 422, an in-flight duplicate is a 409. Records expire
after 24 h.

## Scheduled statuses

Payloads live in `scheduled_statuses.params`; media reservations in
`scheduled_status_media` (one row per attachment — the canonical
reference, so orphan cleanup never deletes reserved media). The
`ScheduledStatusPublish` handler locks the row `FOR UPDATE`, refuses
early publication after a reschedule (re-enqueuing the remaining
delay), and publishes by re-running the create pipeline in the same
transaction that moves media references and deletes the row. A cron
sweep (`content.scheduled_status_sweep_schedule`) re-enqueues due rows
`FOR UPDATE SKIP LOCKED`; per-user scheduled deletion sweeps
`statuses.scheduled_deletion_at` the same way
(`content.scheduled_deletion_sweep_schedule`).

## Polls

Voting serializes on the poll row (`SELECT … FOR UPDATE`): all vote
inserts and `votes_count` bumps commit or roll back atomically.
Duplicate votes, own-poll votes, expired polls, out-of-bounds and
duplicate-in-request choices are 422s.

## Media

`media_attachments/{account}/{uuid}.{ext}` via the single
`store_status_media` seam (epic 18 plugs S3/remote caching in here).
Images run the shared `process_image` pipeline (sniff → bounded
decode → EXIF strip → blurhash + dimensions); audio/video store raw —
no transcoding. `focus` coordinates persist on the row and render as
`meta.focus`. `CleanupOrphanMedia` removes attachments referenced by
neither a status nor a scheduled status past
`media.orphan_retention_days`. On `PUT /api/v1/statuses/{id}`,
`media_ids` follows Mastodon edit semantics: omitted keeps the
current attachments, a supplied list (including an empty one)
replaces them, and ids already attached to that status may be
resubmitted.

## Link previews

`FetchLinkPreview` extracts the first eligible URL (skips own-instance
and disallowed schemes) — never when media is attached — fetches via
pubby's SSRF-guarded client, parses Open Graph (with `twitter:` /
`<title>` fallbacks), stores a `previews/` card image, and upserts
`preview_cards`. Metadata caches in Redis for
`content.link_preview_cache_hours`. Fetch failures ack — never a
retry storm.

## ActivityPub

Outbound `Create`/`Update`/`Delete` are built through pubby's
`OutboxProcessor`; `source` (markdown round-trip), poll fields
(`oneOf`/`anyOf`/`endTime`) and the public-quote interaction policy
are injected into the emitted object. `GET
/ap/users/{u}/statuses/{id}` is visibility-scoped: public/unlisted use
the shared doc-cache under `authorized_fetch`; followers-only/direct
require a verified signer passing `can_see`.

Inbound `Create`/`Update`/`Delete` for Note/Question materializes
`statuses` rows (`local = false`): sanitized `content_html`, explicit
visibility inference from combined activity+object addressing, and
mention/tag/media/poll sync. The ownership gate requires
`activity.actor == attributedTo`, and Update/Delete must match the
stored `account_id` — spoofed mutations are dropped.
