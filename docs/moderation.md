# Moderation

Reports, user-level moderation actions, instance domain blocks and the
audit log.

## Reports

- `POST /api/v1/reports` — authenticated users file reports:
  `account_id`, optional `status_ids[]` (each must belong to the
  target; one `reports` row is created per status — the schema's
  `status_id` is single-valued), `comment` (≤
  `content.max_report_comment`, default 2000), `category`
  (`spam|violation|legal|other`), `forward`.
- `forward=true` on a remote target sends a `Flag` activity to the
  target's instance, attributed to and signed by the instance actor —
  the reporter's identity is not disclosed. Only reports propagate —
  blocks and suspensions are never federated.
- Inbound `Flag` activities create `reports` rows with the remote
  actor as reporter; domains with `reject_reports` (or `suspend`)
  are dropped.
- Moderator queue (`admin`/`moderator` roles via
  `require_moderator_scope`):
  - `GET /api/v1/admin/reports` (`resolved`, `account_id`,
    `target_account_id`, `limit`, `max_id`, `since_id`)
  - `GET /api/v1/admin/reports/{id}`
  - `POST /api/v1/admin/reports/{id}/{resolve,reopen,assign_to_self,unassign}`
- Filing a report sends an `admin.report` notification to every local
  admin/moderator except the reporter.

## User actions

`POST /api/v1/admin/accounts/:id/action` (moderators and admins) with
Mastodon's vocabulary plus Yonder's `delete`:

| `type` | Effect |
|---|---|
| `none` | Warning only — records the action. |
| `sensitive` | Sets `accounts.force_sensitive`; new posts/media are marked sensitive regardless of author preference. Reversed by `unsensitive`. |
| `disable` | Sets `accounts.disabled_at` and revokes tokens/sessions — frozen: login and token use rejected, content intact, reversible by `enable`. |
| `silence` (alias `limit`) | Sets `accounts.limited_at`; hidden from public/federated/tag timelines and trends, visible to followers in home, still findable via search. The profile timeline stays served — the owner and followers see it directly, and the SPA shows non-followers a "limited" notice with a "Show posts anyway" reveal (posts are hidden, not stripped — Mastodon's client-side contract). Follows it sends become follow requests. Reversed by `unsilence`. |
| `suspend` | Sets `accounts.suspended_at` and revokes tokens/sessions — hidden everywhere, login rejected. Reversed by `unsuspend` (access is *not* restored). |
| `delete` | Admin only — immediate account-deletion purge. |

Body: `type`, `report_id` (links and resolves a report), `text`
(reason ≤ 2000), `send_email_notification`.

Reversals are Mastodon's dedicated verbs:
`POST /api/v1/admin/accounts/:id/{unsilence,unsuspend,unsensitive,enable}`.

`POST /api/v1/admin/accounts/:id/role` (admin only, Yonder extension —
Mastodon has no role API) sets `role` to `user|moderator|admin` and
refuses to demote the last *active* admin. `GET /api/v1/admin/accounts/:id`
returns the moderation-state view (`suspended`, `limited`, `disabled`,
`force_sensitive`).

Guards: nobody can act on their own account; moderators can't act on
staff targets (admins or other moderators) — applies to the reversal
verbs too, so a moderator can't undo an admin's action on staff;
`suspend`/`disable`/`delete` refuse the last active local admin; and
a domain block on the instance's own domain is rejected. Guard checks
and the mutations they gate run under a shared transaction-scoped
advisory lock, so concurrent requests can't both pass a last-admin or
staff check.

## Instance domain blocks

`domain_blocks` follows Mastodon's model (migration 0020): a single
`severity` enum plus independent booleans — not a set of orthogonal
flags, because `silence`/`suspend` are supersets of the other effects.

| Field | Values / effect |
|---|---|
| `severity` | `noop` (reject-only, no account restriction), `silence` (Mastodon's "limit": hidden publicly, followers keep it in home, inbound follows become requests), `suspend` (defederation: no traffic either way, content/accounts vanish everywhere) |
| `reject_media` | Remote media attachments from the domain are not cached (avatars/headers/emoji are not cached locally — nothing to gate) |
| `reject_reports` | Inbound `Flag`s dropped |
| `obfuscate` | Mask the domain's middle characters in the public listing |
| `private_comment` / `public_comment` | Admin-only / optionally-public notes |

Admin CRUD (admin role only): `POST/GET /api/v1/admin/domain_blocks`,
`GET/PATCH/DELETE /api/v1/admin/domain_blocks/{id}` — Mastodon's field
names exactly, `digest` = SHA-256 of the domain. POST upserts on the
normalized domain (`https://Example.com:443/x` == `example.com`).

Bulk transfer is available at `GET /api/v1/admin/domain_blocks/export`
and `POST /api/v1/admin/domain_blocks/import` (multipart `file` or
`data`). Export uses Mastodon v4.1+'s exact CSV header:
`#domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate`.
Private comments stay staff-internal and are not exported. Imports accept
that shape, unprefixed equivalents (`domain,severity,...`) with an optional
`private_comment` column, and headerless one-domain-per-line lists of the
kind commonly shared for Pleroma/Akkoma. Headerless rows default to
`suspend`; the Akkoma/Pleroma `reject` severity alias also maps to
`suspend`, while `limit` maps to `silence`. Existing domains are skipped
unless the multipart `overwrite=true` field is sent — imports are
additive by default. The same service backs
`yonder domain-block import <PATH> [--overwrite]` for operators; the CLI
uses `--yes` for non-interactive confirmation when a file contains suspend
rows or `--overwrite` is passed. Invalid rows are reported in the response
without aborting valid rows, rows are capped at 20,000 (Mastodon's limit)
and `admin.max_import_size` bounds uploads, and the batch is atomic for
the valid rows.
Imports that create or update rows write one aggregate
`domain_block.import` audit row.

The `/admin/domain-blocks` panel can also import the public limitations of
another instance directly in the browser. It first tries the remote
`GET /api/v1/instance/domain_blocks` endpoint and falls back to the
Pleroma/Akkoma federation transparency metadata in `GET /api/v1/instance`
(`reject` maps to `suspend`, `federated_timeline_removal` to `silence`).
Retrieved entries open in a selection modal, selected by default, where the
operator can select or deselect rows, keep each remote severity or force one
severity for the import, and either keep the remote reasons or enter one
custom public reason for all selected rows. The browser then submits the
selection through the same bulk CSV import path (with the source instance
recorded in the private note), so existing rows are skipped unless overwrite
is enabled. This discovery only works when the remote instance publishes its
blocks and allows browser access to those public API endpoints.

Public listing `GET /api/v1/instance/domain_blocks` returns domain
(obfuscated when marked), severity and `public_comment`; gated by
`instance.expose_moderated_instances` (default `true`) → 404 when off.

`GET /api/v1/instance/peers` lists the domains the server is aware of
(remote account domains ∪ delivery targets ∪ relays), matching
Mastodon's `Instance.searchable` semantics — `silence`/`suspend`
blocks hide their domain from the listing. Gated by
`instance.peers_api_enabled` (default `true`) → 404 when off.

### Enforcement map

| Path | Mechanism |
|---|---|
| Inbound activities | `activitypub/inbox.rs` rejects suspended domains at the edge (live DB check). |
| Outbound fan-out | `ApCore.blocked` → pubby `blocked_instances`; refreshed on every block write (`blocks::refresh_blocked`). |
| Delivery | `FederationDelivery` job marks rows `failed` for suspended domains at send time. |
| Timelines | `timelines/filter.rs`: `suspend` excluded everywhere, `silence` + `limited_at` from public scopes only (the `Account` profile scope is exempt). |
| Search | `search/statuses.rs`: `suspend` + `silence`; `search/accounts.rs`: `suspend` only (Mastodon: limited accounts stay findable). `search/resolve.rs` refuses suspended lookups. |
| Trends | `tags/trends.rs`: `suspend` + `silence` + `limited_at`. |
| Remote media | `posts/remote.rs` enqueue + `media/jobs.rs` fetch both check `reject_media`/`suspend`. |
| Inbound reports | `handle_flag` drops `reject_reports`/`suspend` domains. |
| Inbound follows | `follow_policy` returns `Manual` for silenced followers (pubby seam). |
| Account visibility | `VISIBLE`/`load_many` exclude `suspended_at`; token resolution rejects `suspended_at` + `disabled_at`. |

All enforcement checks are live queries — blocks take effect without
a restart. One exception: `ApCore.blocked` (pubby's
`blocked_instances` fan-out list) is a per-process snapshot refreshed
only in the process handling the block write — the *worker* process
keeps its startup list until restart. Delivery-time and inbox checks
still consult the database live, so a stale worker snapshot only
affects the recipient fan-out, never actual delivery.

Domains are stored in punycode — the same serialization
`accounts.domain` holds for remote actors — so IDN blocks
(`βόλοσ.example` == `xn--nxasmq6b.example`) enforce correctly.

## Audit log

Every moderation mutation writes exactly one `audit_log` row via
`moderation::audit::record` (who, dotted action, target, reason,
details). Retention is a cron job (`JobKind::AuditLogCleanup`,
`admin.audit_log_sweep_schedule`, default `0 4 * * *`) deleting rows
older than `admin.audit_log_retention_days` (365).
