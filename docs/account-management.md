# Account management

Account lifecycle — moves, deletion, archive import/export, invite links and
registration auto-follow. All endpoints live under `/api/v1/yonder/…` —
Mastodon exposes these operations only through its web UI, so they are Yonder
extensions, not Mastodon-compatible API.

## Moves

- `GET/POST/DELETE /api/v1/yonder/aliases` — manage the caller's
  `alsoKnownAs` URIs. Adding an alias publishes an actor `Update`.
- `POST /api/v1/yonder/move` `{acct, password}` — password-confirmed
  redirect. The target must already name this account in its
  `alsoKnownAs` (fresh-fetched for remote targets). Sets
  `accounts.moved_to_account_id`/`moved_at`, publishes `Update` + a
  `Move` activity addressed to followers, then the `MigrateFollowers`
  job migrates local followers (`Follow` target → `Undo(Follow)`
  origin) in 100-row batches. Followers who block or mute the target
  stay. A 30-day cooldown applies, Mastodon-style (429).
- `DELETE /api/v1/yonder/move` `{password}` — cancels the redirect
  (already-migrated followers stay).
- `moved` on Account entities is the nested target Account, one level
  deep, never recursive.

Inbound `Move`s are never trusted: the inbox enqueues
`MigrateFollowers`, which fetches the target document fresh and
requires `alsoKnownAs ∋ origin` before any write. No `account_aliases`
rows are created from inbound activity.

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
  `filters` download inline (CSV/`filters.json`); `archive` returns
  `202` and is built by the `Export` job into a zip under the private
  `exports/` prefix (never served by `/media`).
- `GET /api/v1/yonder/exports[/{id}[/download]]` — status and the
  authenticated download. Exports expire after 7 days
  (`ExportCleanup` job); one archive request per 7 days per account
  (429 + `Retry-After` — failed exports don't count). `ExportCleanup`
  also fails `running` transfer rows abandoned for >24 h and deletes
  their stored uploads.
- `POST /api/v1/yonder/imports` (multipart: `kind`, `mode`, `file`) —
  validates and stores the upload, returns `202`; the `Import` job
  applies rows in bounded batches (500 rows or ~60 s per run,
  re-enqueueing on a persisted cursor). Progress is on
  `archive_imports` (`processed`/`failed`/`total`). `GET
  /api/v1/yonder/imports[/{id}]` reads it.

Kinds match Mastodon file names (`following_accounts.csv`,
`blocked_accounts.csv`, `muted_accounts.csv`, `domain_blocks.csv`,
`bookmarks.csv`, `lists.csv`); `followers` is export-only; `filters`
uses a `yonder` JSON shape since Mastodon has no CSV. `mode=overwrite`
replaces the account's existing set with the file's; `merge`
(default) unions. Rows are capped at 20 000 per file and
`admin.max_import_size` bounds uploads.

Imported posts (archive `outbox.json`) get fresh local URIs — the
source id lands in `statuses.original_id` (deduped by
`uix_statuses_account_original`), the source timestamp in
`original_created_at`/`created_at`, and nothing is federated.

## Invites

- `POST /api/v1/yonder/invites` `{max_uses?, expires_at?|expires_in?}`
  — create a link (shareable as `{base}/invite/{code}`). Staff always
  may; regular users when `auth.invite_links_enabled` is effective —
  unset means "on only while registration is open".
- `GET /api/v1/yonder/invites`, `DELETE …/{id}` (owner or staff),
  `GET /api/v1/yonder/invites/check/{code}` (public validity check).

Registration (`POST /api/v1/accounts`) accepts `invite_code`. A code
bypasses `closed` registration and auto-approves under
`approval-required`; use is consumed atomically inside the
registration transaction so `max_uses` can't be exceeded
(`users.invite_id` records which invite signed a user up).

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

`AccountDeletion`, `MigrateFollowers`, `Import`, `Export` and
`ExportCleanup` all route to the **bulk** queue. `MigrateFollowers`
registers only when federation is enabled (it fetches actor
documents); the others degrade to local-only work without AP.
