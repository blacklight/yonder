# Media storage

All stored media goes through `media::storage::MediaStore` — a
backend-agnostic interface (`put`/`get`/`delete`/`list_prefix`) built once
per process (`AppState::media`, `JobContext::media`, `PostContext::media`).
Storage keys are backend-agnostic (`avatars/{account}/{uuid}.{ext}`,
`headers/…`, `media_attachments/{account}/{uuid}.{ext}`, `previews/…`); rows
store the *key*, and `media::public_url` renders the absolute URL at read
time.

## Backends (`media.backend`)

- **`local`** (default): files live under `media.local_path`.
- **`s3`**: an S3-compatible bucket via `aws-sdk-s3`. Requires `s3_bucket`,
  `s3_access_key` and `s3_secret_key` (prefer the
  `YONDER_MEDIA__S3_ACCESS_KEY`/`YONDER_MEDIA__S3_SECRET_KEY` env vars for
  secrets); `s3_endpoint`, `s3_region` (default `us-east-1`) and
  `s3_force_path_style` (default `true`, needed by MinIO and most compatible
  stores) complete the client. Missing bucket/credentials abort startup.

## Serving

`GET /media/{*key}` is registered for **both** backends — on S3 it proxies
the bucket, so `/{base}/media/{key}` is the one URL shape everywhere. The
route allow-lists `avatars/`, `headers/`, `media_attachments/` (three
segments) and `previews/` (two), rejects `..`/absolute paths, derives
`Content-Type` from the key extension, and emits
`Cache-Control: public, max-age=31556952, immutable`.

With `media.s3_public_base_url` set, entity URLs render as
`{cdn}/{key}` instead — point the CDN at the bucket; the proxy route stays
available as a fallback.

Private artifacts (`exports/`, `imports/` keys) are written through the same
store but never pass the serving allow-list. **Warning:** the allow-list
only gates the proxy route — a public CDN serves *every* key in the bucket,
`exports/`/`imports/` included. If you configure `s3_public_base_url`,
either keep those prefixes out of the served bucket (e.g. a private
origin or separate bucket) or accept that archive links are unlisted but
reachable.

## Uploads and limits

`media.max_upload_size` (default 50 MiB) caps status media;
`media.max_avatar_size` (default 2 MiB) caps avatar/header uploads. Image
uploads run the shared pipeline (sniff → bounded decode → EXIF strip →
blurhash); `media.strip_exif` (default `true`) re-encodes so metadata is
dropped by construction — GIF comment/XMP extension blocks are a known gap.

## Remote media

`media.fetch_remote_media` (default `false`):

- **off**: remote attachments keep only `remote_url`; entities serve the
  remote URL directly.
- **on**: ingest enqueues `RemoteMediaFetch` per attachment; the job
  downloads through pubby's SSRF-guarded `HttpClient`, stores bytes under
  `media_attachments/{account}/{uuid}.{ext}` and sets `file_path` (keeping
  `remote_url` as provenance). Re-ingesting a post re-enqueues any row still
  missing `file_path`, so evicted media is re-fetched on next sighting.
  `media_url` prefers the local copy when caching is on. The write-back
  claims the row with a `file_path IS NULL` guard: an object that loses the
  claim (duplicate delivery, row deleted mid-fetch) or whose claim fails on
  a provably-unreferenced row is deleted; a *failed-looking* write-back
  whose outcome is ambiguous (the UPDATE may have committed) leaves the
  object — a recoverable leak is preferable to a permanently broken
  `file_path`, which serving would prefer over the `remote_url` fallback.

Fetch size note: on pubby-sdk ≤ 0.6.0 the guarded client hard-caps
response bodies at 16 MiB, so remote media larger than that can never be
cached (the job retries then dead-letters; serving falls back to
`remote_url`). pubby-sdk 0.6.1 adds `HttpClient::max_body_bytes`; the
fetch cap will be raised to `max_upload_size` once the release ages past
the dependency-policy window.

## Cleanup

- **Orphan uploads** (`CleanupOrphanMedia`, `media.orphan_cleanup_schedule`):
  unattached `media_attachments` rows older than
  `media.orphan_retention_days` are deleted (row + object).
- **Remote cache sweep** (`CleanupRemoteMedia`,
  `media.remote_cleanup_schedule`, default disabled): evicts cached remote
  attachments older than `media.remote_cleanup_max_age_days` **or**, while
  total cached bytes exceed `media.remote_cleanup_max_bytes`, oldest-first.
  Attachments on posts a local user **boosted or quoted**
  (`statuses.reblog_of_id`/`quote_of_id` to a local-authored status) are
  never evicted; local favourites do not protect. Eviction deletes the
  object and clears `file_path` — the row and `remote_url` stay, so serving
  falls back to the remote URL.
- **Replaced profile images** (`DeleteMediaObject`): `PATCH
  update_credentials` replacing or clearing an avatar/header enqueues a
  delayed delete of the old object (24 h grace). The URL is served
  `immutable` for a year and remote servers re-fetch the old `icon`/`image`
  URL until they process the actor `Update`, so an immediate delete would
  404 late consumers. The job re-checks `avatar_path`/`banner_path` at
  execution and skips keys a row still references. **Caveat:** a cache or
  browser that holds the old URL for the full `immutable` lifetime can
  still outlive the grace window — after 24 h the URL 404s regardless.

All three run on the dedicated `cleanup` queue (`Queue::Cleanup`) so long
sweeps can't starve bulk fan-out.

## Backend switching caveat

Keys are backend-agnostic but bytes are not migrated: switching
`media.backend` leaves previously written objects behind. A local→S3
migration tool is a follow-up.
