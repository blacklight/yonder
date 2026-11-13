# Posting

Mastodon-compatible status creation, rendering, media, polls,
scheduling, link previews and ActivityPub federation.

## Endpoints

| Method | Path | Scope | Notes |
| ------ | ---- | ----- | ----- |
| POST | `/api/v1/statuses` | `write:statuses` | create; `Idempotency-Key` replays |
| GET | `/api/v1/statuses/{id}` | — | `can_see` gated |
| GET | `/api/v1/statuses/{id}/context` | — | ancestors + descendants, `can_see` gated |
| POST | `/api/v1/statuses/{id}/fetch_remote_context` | `read` | lazy remote-thread backfill (Yonder ext.) |
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
  `script`/`style`/`iframe`; `a[href]`/`img[src]` restricted to
  `content.valid_url_schemes`) → text-node enhancement that linkifies
  bare URLs/`#tags`/`@mentions`, skipping `code`/`pre`/`a`/`small`
  subtrees. `![alt](url)` renders inline as an `<img>` with the alt
  text repeated in a visible caption that links to the image URL
  (Mastodon-style, so the alt doubles as the "open image" link).
- CW/`spoiler_text` is always escaped plain text.
- Rendered HTML is stored in `statuses.content_html`; inbound remote
  HTML is re-sanitized through the same allowlist — with `img`
  permitted only so that recognized inline images survive (below).
- Inbound remote inline images: an `<img>` in remote `content` stays
  inline when the same object also delivers that URL as an image
  `attachment` — recognized by `mediaType: image/*` or the
  ActivityStreams `type` (`Image`, used by SocialHome/PyFed without a
  MIME type). Yonder↔Yonder round-trips our own wrapper markup
  without duplicating captions; converted long-form types
  (`Article`/`Page`/`Audio`/`Video`/`Event` — Friendica
  articles carry images only in `content`). Anything else is dropped,
  exactly as before. Kept images get the same
  `data-yonder-inline-image` wrapper + visible alt caption linking to
  the image URL, and the
  matching `attachment` does **not** also become a
  `media_attachments` row — the image shows once, inline.
- An omitted `content_type` resolves per account: the stored
  `accounts.default_content_type` preference (set via
  `PATCH /api/v1/yonder/settings` `default_content_type` or
  `update_credentials` `source[content_type]`), else the instance
  default `content.default_post_content_type`.

## Visibility

Stored: `public`, `unlisted`, `followers_only`, `direct`, `local`
(input `private` normalizes to `followers_only`; wire output maps back
to `private`). `can_see` enforces author/followers/mention/public
semantics (a mention grants audience even on follower-gated posts —
otherwise the addressee could never reply); the same predicate guards
the poll endpoints (a poll is only as visible as its status) and the
AP object GET — private objects are never served from the shared
doc-cache, and unsigned or unauthorized fetches of them get a 404,
never a revealing 401. Replies to a `direct` status stay in the
conversation: an omitted `visibility` inherits `direct`, an explicit
wider audience is a 422, and the same guard applies to edits via the
stored parent.

`local` posts are instance-only (Akkoma/Songhive semantics): any
authenticated *local* account (`accounts.domain IS NULL`) can read
them — anonymous and remote-account viewers cannot — and they never
federate. `publish_create`/`publish_update` return early for them (no
`activities` row, no deliveries, no outbox entry), the AP object GET
treats them like other gated documents (a verified signer must still
resolve to a local account via `can_see`), and boosts are rejected
outright since boosts only target `public`/`unlisted`. Mentions still
address nominal `to` recipients so local notifications work, but no
AP traffic results. Replies to a `local` parent are pinned the same
way: omitted visibility inherits `local`, `direct` is allowed as the
narrower audience, anything wider is a 422. Quoting a `local` target
pins the quote to `local` for the same reason — the `RE:` link and
quote URI would otherwise ride a federated document. Public feed
documents (RSS/Atom list feeds) exclude `local` even though the list
owner is the viewer.

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

Markdown inline images (`![alt](url)`) are not `media_attachments`
rows — they live in the post HTML only. For federation, the emitted
ActivityPub object additionally lists each distinct image URL as a
standard `Image` attachment (`name` = alt text, `mediaType` sniffed
from the extension), so Mastodon/Pleroma/Akkoma-style clients that
strip inline `<img>` still receive the media.

## Math formulas

With `[math] enabled = true` and a configured `math.command`
(disabled by default), LaTeX delimiters in post text — `$…$`,
`$$…$$`, `\(…\)`, `\[…\]` — are detected on the raw source before
Markdown/plain-text rendering (so multi-line `aligned` blocks keep
their `\\` row separators and backslash commands verbatim; code,
link text/destinations, raw HTML and URLs are skipped, matching the
HTML-level pass used for remote `text/html` posts) and rendered to
images by the external command. Each
distinct formula becomes a real `media_attachments` row
(`math_source = "{mode}:{latex}"` marks it), so:

- the post HTML shows the formula inline (`<img>` inside a
  `data-yonder-math-image` span — `inline` flows with the text,
  `display` centers as a block),
- the media gallery hides it — `StatusMedia` filters rows carrying
  `yonder.math_source` since the formula already renders in the body.
  Formula images are bounded by `math.max_formulas`, **not**
  `media.max_attachments_per_post` — they are inline content, not
  uploads, so a photo-heavy post can't starve them (and vice versa).
- the ActivityPub `attachment` list carries it as a standard `Image`
  document — Mastodon and other clients without formula support see
  the rendered image. Inside the `content` markup each formula is also
  followed by `<a data-yonder-math-alt>` linking to the image, its
  text a paper-style numbered reference — `(1)`, `(2)`, … — counted
  per rendered formula in document order: clients that strip `<img>`
  (Mastodon) still get a compact clickable formula reference. The
  LaTeX source stays available as the `<img>` `alt` text and the
  attachment's `description`. Yonder hides the link visually (the
  `<img>` is the visible formula) but keeps it for assistive tech.

Renderer contract: `<command>… inline|display` (argv split on
whitespace, no shell), LaTeX source without delimiters on stdin, PNG
bytes on stdout. Non-zero exit, timeout
(`math.timeout_seconds`) or empty output leaves the formula as
literal text — a broken renderer never fails a post. Limits:
`math.max_formulas` distinct formulas per post, `math.max_input_chars`
per formula. Repeated identical formulas share one render and one
attachment. Edits and remote `Update`s reuse rows whose
`math_source` still matches (same image URL), and formulas removed
from the text have their attachments detached. Applies to local and
federated posts alike; see `docs/media.md` for storage details and
`config.toml.example` for the command contract.

### Renderer options

The contract is deliberately generic — **any** command honoring
stdin-LaTeX → stdout-PNG works, and the trailing `inline|display`
argument may be ignored by wrappers that don't need it. KaTeX/MathJax
are *not* required; pick whichever fits your platform:

- **Real LaTeX (most compatible)** — `scripts/latex-math-render.sh`
  wraps `tectonic` (single static binary, self-downloads its TeX
  bundle) or `pdflatex` (TeX Live) and rasterizes the PDF with
  `pdftoppm` (poppler-utils) or `mutool`. Full LaTeX/MathJax command
  coverage. Heaviest option (~100 MB with tectonic, ~500 MB+ with
  texlive). Install hints are in the script header.
- **MathJax v3 (mathstodon-style)** — `scripts/math-renderer-example.mjs`
  uses the `mathjax-full` Node library + `sharp` to rasterize the SVG
  output. Requires Node.js; closest match to how mathstodon renders.
- **KaTeX** — same pattern as the MathJax example with the `katex`
  package instead; KaTeX covers a smaller LaTeX subset but is faster.
- **Pre-made converters** — tools like `latex2png`/`tex2png`
  (LaTeX→PNG wrappers around `latex`+`dvipng`) or `math2image`
  (MathJax-node based) work if they honor the stdin/stdout contract;
  most expect a file argument, so wrap them in a small script that
  feeds stdin and `cat`s the PNG to stdout.
- **Typst** — `typst compile` emits PNG directly and is a single
  ~30 MB binary, but its math syntax is LaTeX-*like*, not LaTeX —
  many real-world formulas won't parse. Usable for simple content;
  expect literal-text fallback for complex input.

`docs/media.md` covers where the generated files live; the Dockerfile
ships a commented recipe for adding a renderer to the container
image.

**Theming:** formula images are rasterized once with fixed colors, so
they can't be re-rendered per viewer theme. The frontend approximates
dark mode with a CSS `invert(1) hue-rotate(180deg)` filter on the
math `<img>`s. It works best with **transparent-background** renderers
— ghostscript `pngalpha` (see `MATH_PDF2PNG` in
`scripts/latex-math-render.sh`) or the MathJax/sharp example — where
inversion only affects the glyph colors. A white-page renderer
(`pdftoppm` default) inverts to a dark box instead of a light one;
still readable, slightly less polished.

## Link previews

`FetchLinkPreview` extracts the first eligible URL (skips own-instance
and disallowed schemes, and any URL that is the target of a status
mention or hashtag — matching Mastodon, which never cards a
mention/tag anchor) — never when media is attached — fetches via
pubby's SSRF-guarded client, parses Open Graph (with `twitter:` /
`<title>` fallbacks), stores a `previews/` card image, and upserts
`preview_cards`. Page and image fetches send
`{federation.user_agent} (compatible; Mastodon/{v}; +{base_url})` —
providers like YouTube serve a stripped shell page (no OG player
tags, no oEmbed link) to user agents they don't allowlist, and the
`Mastodon` compatibility token is how the fetcher gets the real
metadata page. Metadata caches in Redis for
`content.link_preview_cache_hours`. Fetch failures ack — never a
retry storm. The job is enqueued wherever a status is written: local
create/edit, scheduled publish, and remote materialization — a
fresh `Create` or an accepted `Update` re-runs it, and re-runs
converge `preview_cards` to the status' current eligibility (media
added or URL removed clears the card, a changed URL replaces it).

Pages that declare an iframeable player — `twitter:player` on a
player card (or without an explicit non-player `twitter:card`), or
`og:video*` with `og:video:type` = `text/html` (YouTube, Vimeo, …) —
upgrade the card to `type: "video"`: `preview_cards` stores
`embed_url`/`embed_width`/`embed_height` and the entity carries
`embed_url`, `width`, `height` plus a synthesized `html` `<iframe>`
(the Mastodon card shape). URLs that duplicate the card image or end
in a media extension are rejected — broken implementations sometimes
point player tags at image/video attachments. A `video/mp4`
`og:video` is a raw media file, not a player, and stays
`type: "link"`. The web UI renders
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
instead of replaying pre-upgrade cache entries. The same sweep also
repairs remote `content` flattened before anchors emitted their
hrefs (a `scheme:// rest` signature — the URL split across
Mastodon's invisible-span markup): `content` is re-flattened from
`content_html` and the status re-enqueues for its card.

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

## Embedding

Every public or unlisted status carries an "Embed" menu entry that
opens a dialog with two copiable snippets. Both target the standalone
`/statuses/{id}/embed` page — the SPA route renders a chromeless
status card that fetches the status anonymously (`can_see` still
applies, so followers-only and direct posts 404 there, which is why
the menu entry only appears on public visibilities) and opens every
link in a new top-level tab rather than inside the frame. Quotes are
rendered recursively: `quote.quoted_status` only materializes at entity
depth 0, so deeper levels (an `accepted` quote carrying just
`quote_id`) are lazily re-fetched by the embed page, up to a cap of 4
nested cards. Media attachments, polls (read-only) and the
link-preview card (`status.card`, always rendered as a plain link —
the click-to-play video iframe is not offered inside embeds) all
appear at every level.

- `<script>` — a `<div class="yonder-embed" data-status="{id}">`
  placeholder plus the static `/embed.js` (served from the frontend's
  `public/` directory). The script swaps each placeholder for an
  iframe and keeps its height in sync with the post: the embed page
  reports the card element's measured height (the document's
  `scrollHeight` bottoms out at the viewport and could never shrink)
  through a `{source: "yonder-embed", type: "resize"}` postMessage,
  verified against the script's origin and `event.source`.
- `<iframe>` — a plain `width="100%" height="400"` frame for
  JavaScript-free contexts; the fixed height can be edited.

Incoming Webmentions targeting an embed URL fold under the embedded
status: `resolve_target_status` recognizes `/statuses/{id}/embed` and
Mastodon's `/@{acct}/{id}/embed` alongside the permalink and AP URI
forms. Source verification works for both snippets — the `<iframe>`
carries the target in its `src`, and the `<script>` placeholder wraps
a fallback `<a href>` that doubles as the no-JS link and the backlink
Webmention receivers look for (`embed.js` clears it once the live
frame mounts). The `<script>` snippet also ends with a
`<noscript><iframe>` fallback so browsers that block third-party
scripts entirely still render the fixed-height frame. The embed page
also boots without Web Storage: under Firefox ETP / Tor Browser
third-party storage blocking every `localStorage` access throws a
`SecurityError`, so all storage goes through no-throw helpers
(`utils/storage.ts`) and the frame renders anonymously.

Outbound `Create`/`Update`/`Delete` are built through pubby's
`OutboxProcessor`; `source` (markdown round-trip), poll fields
(`oneOf`/`anyOf`/`endTime`) and the public-quote interaction policy
are injected into the emitted object. `GET
/ap/users/{u}/statuses/{id}` is visibility-scoped: public/unlisted use
the shared doc-cache under `authorized_fetch`; followers-only/direct
require a verified signer passing `can_see`; `local` additionally
requires the signer to resolve to an account local to this instance.

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

### Remote thread fetch (lazy thread expansion)

`POST /api/v1/statuses/{id}/fetch_remote_context` (auth: `read`
scope; remote statuses only — local ones 422; suspended author
domains 403) backfills the thread a remote status belongs to, so
opening a remote reply shows its context without visiting the
origin instance. The SPA's status page calls it automatically for
remote statuses, then re-reads `GET …/context` and swaps in the
refreshed ancestors/descendants.

Two passes over the remote thread, all through the same ingest path
an inbox `Create` takes (`materialize_remote` — the per-origin
`trusted_note` check included):

- **Ancestors**: the status's own object is re-fetched (`inReplyTo`
  isn't stored locally), then the walk climbs. A locally-known parent
  whose `in_reply_to_id` is still `NULL` means the gap continues
  above it — its remote doc is fetched to keep climbing. Fetched
  ancestors ingest root-first so every child finds its parent, and
  the target is re-ingested last, repairing its own `NULL`
  `in_reply_to_id` via the `ON CONFLICT (uri)` upsert.
- **Replies**: breadth-first expansion of the target's and each
  fetched ancestor's `replies` collection (inline items or
  `first`/`next` linked pages). Items materialize by URI — an inline
  object that passes the same-origin trust check for its page
  ingests directly, anything else is fetched from its own origin
  first, so replies by other authors land (unlike outbox sync, no
  ownership check — that's what the collection is for). Already-local
  items are skipped and their subtrees stay lazy: opening them
  expands their own context.

The response is `{imported, scanned, pages, complete}` — `complete`
when the chain reached a root and every `replies` collection
exhausted inside the budgets. Bounds: signed GETs with bounded
backoff on 429/transient failure, a 60s wall-clock deadline, caps of
64 ancestors / 20 collection pages / 100 remote GETs / 10 expansion
depth, and a per-status Redis lock that 429s a concurrent fetch.
Rate limiting has a dedicated `rate_limit.remote_context` bucket
(default 30 per 5 minutes).

**Selection-time expansion.** A plain `GET /context` on a remote
status triggers the same expansion inline — no client-side endpoint
call needed, so stock Mastodon apps, `tut`, and mobile clients get
thread context automatically. Only *logged-in* viewers trigger it;
anonymous reads serve what's stored (a crawler can't turn the
instance into a fetch amplifier). The request-path walk runs under a
quick budget (`remote_context.quick_budget_ms` ~3s,
`quick_max_fetches` GETs, one *linked* replies page per walk —
inline pages cost no GET and don't count); whatever it doesn't
reach continues in a `ThreadExpand` background job under the
configured `descendant_depth`/`descendant_breadth` with the full
GET/page/time caps. A `status_expansions` row records the last pass —
reads inside `expansion_grace_seconds` (default 10min) make no remote
calls at all; after it, a refresh re-fetches the target and
materializes only replies not yet stored. A failed expansion (dead
origin, lock contention) degrades to serving what's stored — never a
5xx/429 — and a ~60s cool-off keeps it from being retried on every
read. Remote `likes`/`shares` (and `quotes` where exposed) collection
totals are recorded per expansion into separate columns — rendered
counts are `GREATEST(local, remote)` so transactional local counters
are never overwritten, refetched at most once per
`counts_grace_seconds` (15min). Clicking a count proxies the origin's
collection page-by-page through `favourited_by` / `reblogged_by`
(Redis-cached, account stubs, no per-like rows; only when the remote
total exceeds the local count).

Descendants can be partial on first open — that's the intended
trade-off against Mastodon's eager full-thread fetch. Anything not
reached stays lazy and expands when *it* is selected, at any depth.
Objects materialized by an expansion go through a quiet ingest path:
no mention notifications (a backfilled thread must not re-notify
about old replies) and no `ThreadPath` triggers (the walk that
fetched them already materializes their path).

**Arrival paths.** A remote reply whose parent isn't stored keeps the
parent URI in `in_reply_to_uri`; when the parent later materializes
(on any ingest path), parked replies relink and the parent's
`replies_count` moves — no fetch needed. Replies by followed authors
(or mentioning a local user) additionally get a `ThreadPath` job that
climbs the ancestors (≤`path_max`, ~15s) then reveals the status —
both the home-timeline fan-out and the streaming `update` wait for
the job so the reply only appears once its parent link exists; the
ingest-time delayed `TimelineFanOut` (~10s) is the slow-walk
fallback, and `ZADD` fan-out is idempotent so firing twice is a
no-op. Stranger replies trigger nothing.

**Pruning.** `CleanupRemoteActivities` hard-deletes context-only
remote statuses in `prune_batch` rounds. It's **off by default** —
set `federation.remote_activity_prune_schedule` to enable it. A row
is eligible when it's remote, untombstoned, stored longer than
`remote_context.retention_days` ago (`fetched_at`, the insertion
time — remote `created_at` is the origin's `published` and can be
years old), not read through `/context` in the window, and carries
no local anchor: no local favourite/bookmark/mention/pin/
notification/reaction, no report, no conversation mute, no filter
entry, no local vote on its poll, and an author no local account
follows. A row is kept when any surviving row references it —
`in_reply_to_id`/`quote_of_id`/`reblog_of_id` or a parked
`in_reply_to_uri`/`quote_uri` — and the keep-set is a fixpoint, so a
kept reply retains its whole ancestor chain. Rows referenced only by
other deleted rows go together (leaf-up). Hard deletes, not
tombstones — a re-referenced row re-hydrates through the normal
ingest on the next expansion or path walk. Candidate scanning uses a
persistent id cursor so a permanently kept head of the table can't
stall the sweep.
