# Admin panel backend

## Settings overlays

`GET /api/v1/yonder/admin/settings` lists every config key with
`toml_path`, `env`, `value`, `source` (`overlay`/`env`/`default`),
`default`, `requires_restart`, `sensitive`, `writable`. `?section=`
filters to a top-level section.

- `GET /api/v1/yonder/admin/settings/{key}` — one key (dotted TOML
  path in the URL segment).
- `PUT …/{key}` `{value}` — validate + store a `config_overlays` row;
  the effective config reloads immediately.
- `DELETE …/{key}` — remove the overlay (idempotent).

Read-only sections (`server`, `database`, `redis`, `jobs`) are listed
but reject writes with 403. Secret keys (`database.url` and
`redis.url`, which carry passwords, plus `auth.secret_key`,
`email.smtp_password`, `http.vapid_private_key`, the translation API
keys, the S3 credentials) are write-only: reads return
`set: true/false`, while `value` and `default` are `null`. Audit
details are redacted, and 422 messages for secret keys do not echo
the submitted value.

Overlay writes and reloads are serialized, so the live config always
reflects the latest stored overlays.

`requires_restart` marks keys consumed by startup-built structures
(CORS, rate-limit buckets, `MediaStore`, `Dispatcher`, `ApCore`, cron
schedules). Everything else takes effect live via the hot-swapped
`AppState::config()` snapshot.

## Audit log

`GET /api/v1/admin/audit_log` (admin-only; Mastodon's equivalent is
`/api/v1/admin/action_logs`). Filters: `action` (prefix),
`actor_account_id`, `target_type`, `target_id`, `since`/`until`
(RFC 3339). `max_id`/`min_id`/`limit` keyset-paginate (limit capped at 40);
`Link` headers carry cursors. A malformed `actor_account_id`, `target_id`,
`since` or `until` is a 422, not a dropped filter. `action` is a
literal prefix (`_` and `%` do not act as wildcards).

Each admin mutation writes one audit row after its effect. If the
process dies between the effect and the audit insert, the effect is
not audited. Effects and audit rows are not in one transaction
across the database, Redis and the job queue.

## Relays

`GET`/`POST /api/v1/admin/relays`, `POST /api/v1/admin/relays/{id}/authorize`,
`DELETE /api/v1/admin/relays/{id}` — manual accept (no `Follow`
handshake). Rows carry `id`, `inbox_url`, `state`, `created_at` and
`updated_at`. Only `accepted` relays receive public-post fan-out
(`federation.allow_relays` gates globally).

## User actions

`POST /api/v1/admin/accounts/{id}/action` now also accepts
`disable_2fa`, `verify_email`, `require_password_reset`.
`POST …/reset_password` returns a one-time link (mailed when
`email.send_password_reset` is on); `POST …/email_reset` always mails
it (payload `force` bypasses the class flag). Both are moderator+ and
reject self-targets. Moderators cannot mint links for admins or
moderators (403), and suspended or deleted accounts are rejected
(422). `send_email_notification` on the action endpoint now enqueues
the `account_status` mail.

Known gaps:

- The link points to `/auth/password-reset/{token}`, but this
  repository does not serve that route yet and nothing redeems the
  `password-reset` token. Links are not usable until the epic-07
  redemption flow lands.
- `users.require_password_reset` is set by the action but not enforced
  at login. Login enforcement belongs to epic 07.
- Reset tokens ride in the queued `EmailSend` job payload until it
  runs (same pattern as registration verify-email). Accepted for now.

## Email

`GET`/`PUT /api/v1/yonder/admin/email_templates/{kind}` for
`verify_email`/`password_reset`/`welcome`/`account_status` (seeded by
migration 0021; `{{var}}` placeholders). `POST …/email/test` `{to?}`
sends inline through the SMTP transport. `EmailSend` is the real
handler now (lettre + SMTP), honouring `send_welcome`,
`send_verification`, `send_password_reset`, `send_account_status`.

Template vars: a template-shaped payload's non-envelope fields are
flattened into vars, plus `instance_name` and `user_handle`. Subject
and text render raw; `html_body` renders with entity-escaped values
(the template itself is trusted). Mail with an `html_body` goes out as
`multipart/alternative` carrying both the text and HTML parts. The
SMTP transport is cached per config — callers that pass a changed
`email.*` config get a fresh transport — and a warning is logged when
credentials would be sent with `smtp_tls` off. A failed
`account_status` enqueue after a moderation action is logged but not
retried.

## WebPush keys

`POST /api/v1/yonder/admin/webpush/generate_keys` mints a VAPID P-256
pair, stores it as `http.vapid_public_key`/`http.vapid_private_key`
overlays, and returns the public key. Rotation invalidates existing
push subscriptions' encryption.

## Frontend

`GET`/`PUT /api/v1/yonder/admin/frontend` reads/sets
`admin.default_frontend` via the overlay path.
