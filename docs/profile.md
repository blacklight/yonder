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

## Profile fields & `rel="me"`

`accounts.profile_fields` is a JSONB array of
`{name, value, value_raw, verified_at}`:

- `name`/`value_raw` — as entered; `source.fields` exposes them.
- `value` — pre-rendered HTML: `render_verified_link` (`<a rel="me">`)
  for URLs whose scheme is in `content.valid_url_schemes`, otherwise
  `render_bio_html`. This is already the AP `PropertyValue` wire shape.
- `verified_at` — RFC-3339 or null; `null` when the value changes,
  preserved while `name`+`value_raw` are unchanged.

PATCH performs **no network I/O**. When URL-valued fields are present
it clears the stamp on changed entries and enqueues
`JobKind::VerifyProfileLinks` (Bulk queue, deduplicated per account for
60 s). The job fetches each unique URL with pubby's SSRF-guarded
`HttpClient` (≤4 in flight per run), parses for
`a[rel~=me], link[rel~=me]` (scraper), and stamps `verified_at` when a
link resolves to the actor id (`{base}/ap/users/{username}`) **or** the
profile URL (`{base}/@{username}`). A definitive non-match or non-2xx
clears the stamp; a transient failure keeps it. Writes go through
`SELECT … FOR UPDATE` and only touch entries still carrying the fetched
`(name, value_raw)` pair. The same job sweeps every local account on
`federation.profile_link_verification_schedule` (default `0 4 * * *`,
`""` disables). Verification flips never emit an `Update` —
`verified_at` doesn't reach the AP document.

## ActivityPub propagation

A profile change publishes `Update{actor}` via
`activitypub::delivery::publish_actor_update`: the per-username handler
LRU is invalidated first (it caches `ActorConfig`), the handler is
re-resolved, `image` (the banner) is injected into the document — pubby
has no `ActorConfig` banner field, and the *served* actor document
still lacks `image` until an upstream change (known divergence), then
pubby stores the activity and fans out through the durable `deliveries`
ledger. A publish failure is logged but doesn't fail the PATCH.

Two upstream pubby follow-ups: `ActorConfig.header_url`/`Actor.image`
(so the served document carries the banner), and
`HttpResponse.headers` (so `Link: <…>; rel="me"` response headers can
count as verification, as Mastodon allows).
