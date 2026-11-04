# Account management

Account lifecycle — moves, deletion, archive import/export, invite links and
registration auto-follow. The canonical endpoints live under
`/api/v1/yonder/…`; Akkoma-compatible adapters (`/api/pleroma/*`,
`/api/v1/pleroma/backups`) expose the same operations to Akkoma/Pleroma
clients. Mastodon exposes these operations only through its web UI, so the
Yonder routes are extensions, not Mastodon-compatible API.

## Moves

- `GET/POST/DELETE /api/v1/yonder/aliases` — manage the caller's
  `alsoKnownAs` URIs. Adding an alias publishes an actor `Update`.
- `GET /api/v1/yonder/move` — the caller's move state: redirect,
  30-day cooldown remainder, and follower-migration progress.
- `POST /api/v1/yonder/move` `{acct, password}` — password-confirmed
  redirect. The target must already name this account in its
  `alsoKnownAs` (fresh-fetched for remote targets). The redirect,
  `migration_events` row and publication work commit in one
  transaction; the `PublishMove` job delivers `Update` + a signed
  `Move` (stable activity id `"{actor}#moves/{event_id}"`) through the
  delivery ledger with retries, then `MigrateFollowers` migrates local
  followers (`Follow` target → `Undo(Follow)` origin) in 100-row
  batches. Per-follower outcomes are durable — a failed follow retries
  with capped backoff (5 retries) instead of being advanced past, and
  the event ends `failed` rather than silently dropping it.
  Followers who block or mute the target stay (recorded as `skipped`
  outcomes); their private notes, mutes, blocks and list memberships
  carry over. A 30-day cooldown applies from initiation, Mastodon-style
  (429).
- `DELETE /api/v1/yonder/move` `{password}` — cancels the redirect
  (already-migrated followers and delivered Moves stay).
- `moved` on Account entities is the nested target Account, one level
  deep, never recursive. Local followers get a `move` notification
  (Akkoma-style extension) once per migration event; the old profile
  keeps rendering with a destination banner instead of redirecting.
- Akkoma adapters: `POST /api/pleroma/move_account` `{password,
  target_account}` and `GET/PUT/DELETE /api/pleroma/aliases` `{alias}`
  (mutations return `{"status":"success"}`), plus `POST
  /api/pleroma/{follow,blocks,mutes}_import` `{list}` which queues the
  same import job as a CSV upload.

Inbound `Move`s are never trusted: the inbox verifies signature
ownership and `actor == object`, takes a per-origin advisory lock so
replays coalesce onto one `migration_events` row, fetches the target
document fresh (cache invalidated), and requires `alsoKnownAs ∋
origin` before any write — transient fetch failures retry instead of
failing verification. No `account_aliases` rows are created from
inbound activity. A new Move activity for a terminal pair restarts
migration from cursor 0; already-migrated followers are skipped by
idempotent follow writes.

## Credentials

- `POST /api/v1/yonder/account/password` `{current_password,
  new_password}` — self-service password change. Re-authenticates
  with the current password (403 on mismatch), enforces the
  registration minimum length (422), then rotates
  `users.password_hash` and revokes every other access token along
  with its refresh tokens and session rows. The caller's own token
  survives so the active session isn't dropped — the Mastodon web
  behaviour.

## Deletion

- `POST /api/v1/yonder/account/delete` `{password}` — schedules
  deletion. The account is deactivated immediately (suspended, all
  tokens/sessions/authorizations revoked). With the default
  `admin.account_deletion_grace_days = 0` the `AccountDeletion` job is
  enqueued at once; a non-zero grace leaves the account deactivated
  until the `admin.account_deletion_sweep_schedule` cron picks the due
  `account_deletions` row up. Cancellation is admin/CLI-only and only
  while `state = 'pending'`; it lifts the deactivation only when the
  deletion itself suspended the account (`was_suspended` keeps a prior
  moderator suspension in place).

The purge emits `Delete(actor)` (`{actor}#delete`, to Public, cc
followers, plus the inboxes of remote accounts the user followed),
then **tombstones**: the `accounts` row keeps username, `actor_id` and
the key pair (in-flight deliveries still sign; the username can never
be re-registered), redacts profile fields, deletes `users` (email and
credentials cascade), statuses, relationships and all owned rows, then
the account's media under `avatars/`, `headers/`, `exports/`,
`imports/`, `media_attachments/`.

## Import/export

- `POST /api/v1/yonder/exports` `{kind}` — `following` | `followers` |
  `blocking` | `muting` | `domain_blocks` | `bookmarks` | `lists` |
  `filters` download inline (CSV/`custom_filters.json`); `archive`
  returns `202` and is built by the `Export` job into a zip under the
  private `exports/` prefix (never served by `/media`).
- `GET /api/v1/yonder/exports[/{id}[/download]]` — status and the
  authenticated (`no-store`) download. Exports expire after 7 days
  (`ExportCleanup` job); one archive request per 7 days per account
  (429 + `Retry-After` — failed exports don't count, and the check +
  insert serializes on a per-account advisory lock). `ExportCleanup`
  also fails `running` transfer rows abandoned for >24 h and deletes
  their stored uploads.
- Akkoma adapter: `GET/POST /api/v1/pleroma/backups` behind
  `read:backups` — index/create in Akkoma's `{id, content_type, url,
  file_size, processed, inserted_at}` projection. `url` is a
  1-hour HMAC-signed, bearer-free download link
  (`/api/v1/pleroma/backups/{id}/download?expires&signature`), minted
  only into the owner's own list. The link outlives a suspension (the
  restricted export surface allows it) but stops working if the
  account is disabled, deleted or pending deletion. Expired archives
  are excluded from the index. Signing shares the
  `auth.secret_key`-derived material, so the index fails closed (500)
  when the key is unset. The signature sits in the query string, so
  access logs record a live link for up to an hour; treat those logs
  as sensitive or redact the query.
- `POST /api/v1/yonder/imports` (multipart: `kind`, `mode`, `file`,
  optional `federate`) — validates and stores the upload, returns
  `202`; the `Import` job applies rows in bounded batches (500 rows or
  ~60 s per run, re-enqueueing on a persisted cursor, heartbeating
  `archive_imports.updated_at`). Progress is on `archive_imports`
  (`processed`/`failed`/`total`, `category_state`, `summary`). `GET
  /api/v1/yonder/imports[/{id}]` reads it; `POST …/{id}/cancel` and
  `…/{id}/resume` control it. The `ImportSweep` cron recovers lost job
  envelopes, overdue retries and dead runs.

Kinds match Mastodon file names (`following_accounts.csv`,
`blocked_accounts.csv`, `muted_accounts.csv`, `blocked_domains.csv`,
`bookmarks.csv`, `lists.csv`, `custom_filters.json` with
`keywords_attributes`/status associations); `followers` is
export-only; the old `domain_blocks.csv`/`filters.json` names stay
accepted on import. `mode=overwrite` replaces the account's existing
set with the file's; `merge` (default) unions. Rows are capped at
20 000 per file and `admin.max_import_size` bounds uploads.

Archive imports accept Mastodon ≥4.2 ZIP, pre-4.2 `.tar.gz`,
Akkoma/Pleroma and Yonder formats (detected by magic bytes, with
traversal/bomb/member limits; `*.pem` key members and LD `signature`
blocks are ignored). Imported posts (archive `outbox.json`, Create and
Announce) get fresh local URIs — the source id lands in
`statuses.original_id` (deduped by `uix_statuses_account_original`)
and the source timestamp in `original_created_at`/`created_at`.
Federation of imported public posts is the explicit per-import
`federate` option: off means zero import-generated activities;
on delivers each authored event once through the `publication_work`
drain with deterministic ids and original `published` dates.

Remote (OAuth/API) imports: `POST /api/v1/yonder/import_connections`
begins an authorization-code + PKCE flow on the source instance.
`origin` accepts an absolute `https://` URL, a bare domain, or an
`@user@host` handle (the host becomes the origin). It must carry no
path, query, fragment or credentials. Consent requests only the
scopes for the selected categories, so at least one category is
required. The callback at `…/import_connections/callback` redirects
to `/settings/data`, or to a validated local `return_to` path
(single `/`-prefixed, printable ASCII, no `\`, `?` or `#`). Both
outcomes use the same destination: `?connected=1` on success,
`?import_error=forbidden|invalid|failed` on failure. Remote
credentials are stored XChaCha20-Poly1305-encrypted under an HKDF
domain derived from `auth.secret_key` (imports fail closed when it is
unset). `POST /api/v1/yonder/import_sessions` starts a resumable
session with per-category selection (`statuses`, `dms`, `bookmarks`,
`favourites`, `following`, `blocks`, `mutes`, `profile`,
`observations`), cap/order/range and the same immutable `federate`
choice; 401 pauses for reauthorization and `ImportSweep`/`resume`
recover transient failures. Received interactions become
owner-scoped `historical_observations` (never fabricated activities),
readable via `GET /api/v1/yonder/statuses/{id}/history` and
`GET /api/v1/yonder/history/messages`, and surfaced on status
entities under `yonder.imported`.

## Invites

- `POST /api/v1/yonder/invites` `{max_uses?, expires_at?|expires_in?}`
  — create a link (shareable as `{base}/invite/{code}`). Staff always
  may; regular users when `auth.invite_links_enabled` is effective —
  unset means "on only while registration is open". The web UI
  exposes create/list/revoke under **Settings → Invite links**
  (`/settings/invites`); the create form hides for non-staff users
  when invite links are disabled (`registrations.invites_enabled`
  on `/api/v2/instance`).
- `GET /api/v1/yonder/invites`, `DELETE …/{id}` (owner or staff),
  `GET /api/v1/yonder/invites/check/{code}` (public validity check).

Registration (`POST /api/v1/accounts`) accepts `invite_code`. A code
bypasses `closed` registration and auto-approves under
`approval-required`; use is consumed atomically inside the
registration transaction so `max_uses` can't be exceeded
(`users.invite_id` records which invite signed a user up).

When email verification is required, the confirmation mail carries a
`{base}/verify-email/{token}` link: the SPA route POSTs the token to
`POST /api/v1/accounts/verify_email`, keeping the side effect on POST
so mail scanners prefetching the link can't burn the single-use
token (7-day TTL, Redis-stored).

## Auto-follow

`admin.auto_follow_accounts` are followed by every newly *active*
account; `admin.auto_followed_by` (local accounts only) follows every
newly active account. Runs after open registration and after an
invite auto-approval — under `approval-required`, it runs at approval
time, not sign-up. Remote entries resolve through WebFinger/actor
fetch and follow with real `Follow` activities, never fabricated rows.

## Config keys

| Key | Default | Purpose |
|---|---|---|
| `auth.invite_links_enabled` | unset → open-only | invite links for non-staff |
| `admin.account_deletion_grace_days` | `0` | delay before Delete+purge |
| `admin.account_deletion_sweep_schedule` | `*/15 * * * *` | due-deletion cron |
| `admin.export_cleanup_schedule` | `0 3 * * *` | expired-export cron |
| `admin.max_import_size` | `52428800` | import upload cap (bytes) |

## Jobs

`AccountDeletion`, `PublishMove`, `MigrateFollowers`, `Import`,
`ImportPublish`, `ImportSweep`, `Export` and `ExportCleanup` all route
to the **bulk** queue. `MigrateFollowers` registers only when
federation is enabled (it fetches actor documents); the others degrade
to local-only work without AP.
