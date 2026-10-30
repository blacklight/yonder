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
| POST | `/api/v1/statuses/{id}/translate` | `read:statuses` | Translation entity; `lang` via query or body |
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
semantics (a mention grants audience even on follower-gated posts —
otherwise the addressee could never reply); the same predicate guards
the poll endpoints (a poll is only as visible as its status) and the
AP object GET — private objects are never served from the shared
doc-cache, and unsigned or unauthorized fetches of them get a 404,
never a revealing 401. Replies to a `direct` status stay in the
conversation: an omitted `visibility` inherits `direct`, an explicit
wider audience is a 422, and the same guard applies to edits via the
stored parent.

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
retry storm. The job is enqueued wherever a status is written: local
create/edit, scheduled publish, and remote materialization — a
fresh `Create` or an accepted `Update` re-runs it, and re-runs
converge `preview_cards` to the status' current eligibility (media
added or URL removed clears the card, a changed URL replaces it).

Pages that declare an iframeable player — `twitter:player`, or
`og:video*` with `og:video:type` = `text/html` (YouTube, Vimeo, …) —
upgrade the card to `type: "video"`: `preview_cards` stores
`embed_url`/`embed_width`/`embed_height` and the entity carries
`embed_url`, `width`, `height` plus a synthesized `html` `<iframe>`
(the Mastodon card shape). A `video/mp4` `og:video` is a raw media
file, not a player, and stays `type: "link"`. The web UI renders
video cards click-to-play — thumbnail + play button, the
third-party iframe loads only on demand. Remote platforms that
federate video as ActivityPub `Video` objects (PeerTube) already
arrive as media attachments, not link previews.

Cards stored before a card-shape change don't upgrade on their own
— the job only re-runs on write. `preview_cards.scan_version`
records which card-shape version wrote each row; running
`yonder run-job fetch_link_preview` (empty payload) fans out a
Bulk-queue refresh per stale row, so admins can rescan existing
cards once after upgrading. The Redis metadata envelope is
versioned in lockstep (`preview:vN:`), so the rescan re-fetches
instead of replaying pre-upgrade cache entries.

## Translation

`POST /api/v1/statuses/{id}/translate` returns the Mastodon
Translation entity (`content`, `spoiler_text`, `poll.options`,
`media_attachments[].description`, `detected_source_language`,
`provider`). Target language is `lang` (query or body) → the viewer's
interface locale → `i18n.fallback_locale`. `can_see` is the only
audience gate — any status the viewer can see translates (including
followers-only and direct, whose text is sent to the configured
backend; stricter than that, Mastodon also limits it to
public/unlisted). Results cache in
Redis for 24h keyed by `(status, lang, sha256 of source strings)`, so
edits and alt-text changes invalidate naturally.

`content.translation_backend` picks the provider; the instance
entities report `configuration.translation.enabled` only when it has
what it needs (`google-web` always; `google-api` needs
`google_translate_api_key`; `deepl` needs `deepl_api_key` and honours
`deepl_api_url` — default `https://api-free.deepl.com`, pro keys use
`https://api.deepl.com`; `libretranslate` needs `libretranslate_url`
and honours `libretranslate_api_key`). Requests go through pubby's
sync `HttpClient` in `block_in_place` — relaxed, since the target is
either a hardcoded public host or an admin-configured URL that may
legitimately live on a LAN. The API keys count as sensitive in the
admin settings registry.

## ActivityPub

Outbound `Create`/`Update`/`Delete` are built through pubby's
`OutboxProcessor`; `source` (markdown round-trip), poll fields
(`oneOf`/`anyOf`/`endTime`) and the public-quote interaction policy
are injected into the emitted object. `GET
/ap/users/{u}/statuses/{id}` is visibility-scoped: public/unlisted use
the shared doc-cache under `authorized_fetch`; followers-only/direct
require a verified signer passing `can_see`.

The **web permalinks** — `/@{acct}` (`accounts.url`), `/@{acct}/{id}`
(`statuses.url`) and the SPA's `/statuses/{id}` — content-negotiate on
`Accept`: `application/activity+json`, `application/ld+json` or plain
`application/json` get the AP document (status object through the same
visibility-gated path; actor document via a `dispatch_multi` routed on
the canonical actor path while `request_target` keeps the signed web
URL), every other accept gets the SPA. This is what makes a post URL
searchable on remote instances — a 200 HTML reply is a silent miss in
their resolver. The status acct segment is decorative (id is
authoritative, like Mastodon); `/@{u}@{domain}` serves only when the
domain names this instance.

Inbound `Create`/`Update`/`Delete` for `Note`, `Question`,
`Article`/`Page` and the media/event shapes `Audio`, `Video`,
`Event` (Songhive/Funkwhale tracks, PeerTube videos, Mobilizon
events — all degrade to notes) materializes `statuses` rows
(`local = false`): `content_html` rendered per the object's
`mediaType` — `text/markdown`/`text/x-markdown` and `text/plain`
(PeerTube descriptions) go through the same pipeline locals use,
anything else is sanitized as claimed HTML — explicit
visibility inference from combined activity+object addressing, and
mention/tag/media/poll sync. `summary` is HTML per AS2 — it is
always stripped to plain text: for `Note`/`Question` it becomes the
CW, while the converted types (`Article`/`Page`/`Audio`/`Video`/
`Event`, Mastodon's converted set) get no CW and instead fold the
`name` headline, the teaser and a permalink footer into
`content_html` — WriteFreely articles render as title + `[…]` +
link footer, and the permalink in the text is what the link-preview
job turns into the article card (og:image header image). The
ownership gate requires
`activity.actor == attributedTo`, and Update/Delete must match the
stored `account_id` — spoofed mutations are dropped.
