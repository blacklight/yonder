# User profiles

`PATCH /api/v1/accounts/update_credentials` edits the caller's profile:
display name, bio (`note`), avatar and header images, ordered profile
fields, and the `locked`/`bot`/`discoverable` flags. Requires a
`write:accounts`-covered token. Accepts `multipart/form-data` (the only
transport that carries files), `application/x-www-form-urlencoded`, and
`application/json`. Returns the CredentialAccount — the
`verify_credentials` entity plus `source{note, fields}` with raw
values. An empty `avatar`/`header` part (or `""`/`null` in JSON) clears
the column.

Limits: `content.max_display_name_length` (100),
`content.max_bio_length` (5000), `content.max_profile_fields` (10),
`content.max_profile_field_{name,value}_length` (250/2000), all counted
in characters. Uploads cap at `media.max_avatar_size` (2 MiB) with a
route-level body limit of `2 × max_avatar_size + 64 KiB`.

## Media storage contract

Uploads are sniffed (`image::guess_format` — declared Content-Type is
advisory), dimension/allocation-bounded (4096 px, 128 MiB decode), and
stored under `media.local_path` at `avatars/{account_id}/{uuid}.{ext}` /
`headers/…`. The **key** lands in `avatar_path`/`banner_path`;
`media::public_url` renders `{federation.base_url}/media/{key}` at read
time, so a base-URL or CDN change doesn't invalidate stored data.
`GET /media/{*key}` serves the local backend (allow-listed prefixes,
`immutable` caching). `media.strip_exif` (default on) re-encodes
png/jpeg/webp through `image` — its encoders write no metadata.
**GIFs pass through untouched** to keep animation: they carry no EXIF,
but comment/XMP extension blocks are *not* stripped (known gap). A
blurhash of the first frame lands in `avatar_blurhash`/`header_blurhash`
and surfaces under the account entity's `yonder` extension object.

Replacing or clearing an image does **not** delete the old object
immediately: a `DeleteMediaObject` job removes it after a 24 h grace
delay, because the old URL is served `immutable` for a year and remote
servers keep re-fetching it until they process the actor `Update`
(see `docs/media.md` — the URL still 404s for caches that outlive the
grace window).

## Profile fields & `rel="me"`

`accounts.profile_fields` is a JSONB array of
`{name, value, value_raw, verified_at}`:

- `name`/`value_raw` — as entered; `source.fields` exposes them.
- `value` — pre-rendered HTML: `render_verified_link` for URLs whose
  scheme is in `content.valid_url_schemes`, emitting
  `<a href="URL" rel="me">URL</a>` — the full URL as link text, which
  is exactly the shape Mastodon's remote-field verifier accepts
  (`extract_url_from_html` requires a sole `<a>` with `href` equal to
  its text; a scheme-less label is never fetched). The frontend strips
  the scheme for display. A bare `@user@domain` value renders as
  `<a href="https://{domain}/@{user}">` (Mastodon's mention-field
  shape; the leading `@` keeps it distinct from email values). Submitted
  markup is re-rendered by `sanitize_field_html` (anchors kept as
  safe links — Mastodon accepts `<a>` values — other tags stripped
  with their text kept, `javascript:` hrefs degrade to text);
  otherwise `render_bio_html`. This is already the AP `PropertyValue`
  wire shape.
- `verified_at` — RFC-3339 or null; `null` when the value changes,
  preserved while `name`+`value_raw` are unchanged. A submitted value
  that is a single `<a>` element (e.g. a client echoing the rendered
  `value` back) collapses to its link target before storage when the
  href is a valid URL — markup can't be stored as the raw value, so
  an echo keeps both the stamp and the editable text.

PATCH performs **no network I/O**. When link-valued fields change it
enqueues `JobKind::VerifyProfileLinks` (Bulk queue, unconditional —
a deduplicated enqueue could leave changed fields unverified past
the next read-throttle window). The job resolves each entry's
verification URL the way Mastodon's `value_for_verification` does —
the raw value when it is itself a URL, otherwise the first `<a>`
href inside the rendered `value` — fetches each unique URL with
pubby's SSRF-guarded `HttpClient` (≤4 in flight per run), parses for
`a[rel~=me], link[rel~=me]` (scraper — `<noscript>` wrappers are
stripped first, because html5ever's scripting-enabled parse hides
their contents while Mastodon's gumbo parser sees SPA fallback links
inside `<noscript>`), and stamps `verified_at` when a link resolves to
the actor id (`{base}/ap/users/{username}`) **or** the profile URL
(`{base}/@{username}`). A 2xx page with no matching backlink clears
the stamp — but only after a confirmation fetch also misses: a
single transient "2xx without the link" (CDN edge page, bot wall,
deploy window) must not flap a verified field. Non-2xx responses and
fetch errors keep it, so transient outages never wipe verification.
Writes go through `SELECT … FOR UPDATE` and only touch entries still
carrying the fetched `(name, value_raw)` pair. The same job sweeps
every local account on `federation.profile_link_verification_schedule`
(default `0 4 * * *`, `""` disables). A profile read also re-enqueues
the job while any link-valued field is unverified — covers fields
added before their target page gained the `rel="me"` backlink —
throttled to one attempt per account per 15 minutes via a Redis flag.
Verification flips never emit
an `Update` — `verified_at` doesn't reach the AP document.

For **remote** accounts the same column is populated at ingest: the
actor document's `attachment[]` `PropertyValue` entries (any spelling —
plain, `schema:`-prefixed, or the schema.org IRI; a missing `type` also
counts) map to `{name, value, verified_at}` — `value` is the remote's
pre-rendered HTML. `verified_at` is *verified locally* like Mastodon
does: `RemoteActorFetch` and inbound `Update{Person}` upserts enqueue
`VerifyProfileLinks` for accounts whose doc carries attachments, and
the job checks the field link against the remote actor's own identity —
`accounts.actor_id` (e.g. `https://instance/users/name`),
`accounts.url` (e.g. `https://instance/@name`), **or** the derived
`https://{domain}/@{username}` alias (pages link either spelling of
the same profile). Refreshes carry a field's stamp over when its
**link target** is unchanged — comparing the extracted URL (the
`value_raw` stored for anchor-valued entries, or the first `<a>`
href), not the literal `value` markup, so a remote re-render with
different attributes or spans can't drop verification; a changed
target resets it. Remote rows that predate verification (or never
went through a fetch — the 0028 backfill, partial docs) are caught
by the same read-triggered enqueue local rows get. A document with
no `attachment` key leaves stored fields alone (partial docs must not
clobber); a present-but-empty array clears them. The extraction is
bounded to 32 entries with 255/4096-char name/value caps.
An inbound `Update{Person}` refreshes the row in place — the activity
carries the full actor document and only applies when `object.id`
equals the signer.

## ActivityPub propagation

A profile change publishes `Update{actor}` via
`activitypub::delivery::publish_actor_update`: the per-username handler
LRU is invalidated first (it caches `ActorConfig`), the handler is
re-resolved, then pubby stores the activity and fans out through the
durable `deliveries` ledger. A publish failure is logged but doesn't
fail the PATCH.

The actor document's `summary` renders the raw stored bio through
`render_bio_html` — URLs, `#hashtags` (absolute `rel="tag"` anchors;
relative hrefs are stripped by remote sanitizers) and `@user@domain`
handles. The actor `tag` array (pubby `ActorConfig.tag`) carries a
`Hashtag` entry per bio hashtag plus an `Emoji` entry per local
`:code:` used in the display name, bio or field values — remote
instances render profile emojis and index bio hashtags from it; the
raw `:code:` text stays verbatim in `summary`/`name`/`attachment` for
the remote's own substitution. `docs/emoji.md` covers emoji storage.

One remaining upstream pubby follow-up: `HttpResponse.headers` (so
`Link: <…>; rel="me"` response headers can count as verification, as
Mastodon allows).

## Profile reads (epic 18)

| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/api/v1/accounts/{id}` | optional | Full Account entity: `avatar`/`header`, `acct`, counters, verified `fields`, `created_at`, `last_status_at` (latest non-tombstone status date), `locked`/`bot`, `moved` |
| GET | `/api/v1/accounts/{id}/statuses` | optional | Profile tabs: `pinned`, `only_media`, `exclude_replies` (self-replies stay), `exclude_reblogs`; `max_id`/`min_id`/`since_id`/`limit` pagination with `Link` headers (`pinned` returns the whole pin set, unpaginated) |

Missing, deleted and suspended accounts all return 404; moved accounts
return 200 with `moved` — including remote accounts whose document
carries `movedTo`: a refresh honors it only when the target's
`alsoKnownAs` names the origin (the same contract inbound `Move`
activities are verified against), and the target is fetched when
unknown locally. Remote accounts serve stored counters
(best-effort) and enqueue a `RemoteActorFetch` when `actor_cache` is
stale (>24h) — a stale remote never blocks the read, and repeat reads
are throttled to one enqueue per actor per five minutes. The fetch
job records its outcome in `remote_network_syncs`: a failed attempt
suppresses re-enqueueing for the stale window (the job's own retries
cover transient outages) and a 404/410 marks the actor `actor_gone`,
acked without retry and re-probed weekly. Rows already
enriched before remote `attachment` → `profile_fields` projection
existed are repaired by migration `0028_remote_profile_fields_backfill`
(no network I/O — the stored `actor_data` is reprojected). Freshness
beyond the read path: `federation.remote_actor_refresh_schedule`
(unset by default) sweeps every stale remote actor on a cron — actors
with a fresh failure/gone marker are skipped — and
`yonder run-job remote_actor_fetch --payload '{"actor_url": "…"}'`
re-syncs one account on demand. The
account-statuses scope applies the viewer's visibility rules
(blocks/mutes/domain blocks, follower-gated posts for followers);
anonymous viewers see only `public`/`unlisted`. Two noted behaviors:
`pinned` composes with the other filters (`pinned&only_media` returns
only pinned statuses that carry media), and `last_status_at` counts
every non-tombstone status including `direct` — Mastodon's
account-level semantics. Remote-media caching and cleanup rules are in
`docs/media.md`.
