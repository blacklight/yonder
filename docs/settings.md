# User settings

Persistence and API for the BOOTSTRAP "Users → User settings" surface.
Mastodon-native settings use the standard endpoints; everything else
lives under the `yonder` extension object.

## Where settings live

| Setting | Column | Setter |
|---|---|---|
| `locked`, `bot`, `discoverable` | `accounts` | `PATCH /api/v1/accounts/update_credentials` |
| Hide network (`hide_collections`) | `accounts.hide_network` | `update_credentials` |
| Default visibility (`source.privacy`) | `accounts.default_visibility` | `update_credentials` (`source[privacy]`) |
| Default posting language (`source.language`) | `accounts.default_language` | `update_credentials` (`source[language]`) |
| Default sensitive (`source.sensitive`) | `accounts.default_sensitive` | `update_credentials` (`source[sensitive]`) |
| Search-engine indexing (`indexable`) | `users.indexable` | `update_credentials` |
| Interface language | `users.locale` | `PATCH /api/v1/yonder/settings` (`interface_language`) |
| Theme | `users.theme` | yonder settings (`theme`) |
| Accent color | `users.accent` | yonder settings (`accent`) |
| Stranger notifications | `users.block_stranger_notifications` | yonder settings |
| Scheduled deletion (days) | `users.scheduled_deletion_days` | yonder settings |
| App display | `users.show_app_display` | yonder settings |
| Public posts in search | `users.include_public_posts_in_search` | yonder settings |
| Directory opt-in | `accounts.directory_opt_in` | yonder settings |
| CW always-expand | `users.cw_always_expand` | yonder settings (`cw_always_expand`; read as `reading:expand:spoilers` in preferences) |
| Timeline language filter | `users.filter_languages` | yonder settings |
| Email notifications (6 flags) | `users.email_notif_*` | yonder settings (`email_notifications`) |

`PATCH /api/v1/yonder/settings` also accepts `default_visibility`,
`default_posting_language` and `default_sensitive` so the endpoint is a
complete writable settings surface for the SPA — same columns and
validation as the Mastodon-native `source.*` params.

## Reads

- `GET /api/v1/accounts/verify_credentials` (and the PATCH response):
  Mastodon flags at their canonical positions — `source`
  (`privacy`, `sensitive`, `language`, `note`, `fields`), top-level
  `hide_collections`/`indexable` — plus `yonder.settings`.
- `GET /api/v1/preferences` (`read:accounts`): Mastodon Preferences
  entity — `posting:default:visibility`, `posting:default:sensitive`,
  `posting:default:language`, `reading:expand:media` (`"default"`),
  `reading:expand:spoilers` — plus the same `yonder` settings object.
- `GET /api/v1/yonder/settings` (`read:accounts`): the extension
  object. Every key it returns is PATCH-able.

## Patch semantics

Partial updates only: absent keys are untouched, `null` resets to the
BOOTSTRAP default (NULL for nullable columns), and `""` resets scalar
strings in `update_credentials` form bodies. In JSON bodies booleans
must be `true`/`false`/`null` (never `""`), arrays `[]`. Nested
`source` objects accept `source_*` flat aliases; a nested/flat
disagreement is a 422, never a coin flip.

Validation is strict — unknown keys, unknown values and out-of-range
numbers are 422. Highlights:

- `default_visibility` / `source.privacy`:
  `public`/`unlisted`/`followers_only`/`private`. `direct` is rejected
  on writes; a legacy stored `direct` renders as `private` and logs a
  warning.
- `interface_language`, `default_posting_language`, `source.language`
  and registration `locale`: `en`/`es`/`it`/`de`/`ar` (normalized to
  lowercase).
- `filter_languages`: up to 100 BCP-47-ish tags, lowercased and
  deduplicated — *not* limited to the UI set.
- `scheduled_deletion_days`: integer 0–3650 (0/NULL = never).
- `theme`: `light`/`dark`/`system` (NULL resolves to `system`).
- `accent`: `#rrggbb` or `null`.
- `email_notifications`: `follow` (default false), `follow_request`
  (true), `reblog` (false), `favourite` (false), `mention` (true),
  `quote` (true); partial objects merge, `null` resets all six.

## Language resolution

- `users.locale = NULL` → follow the client's system language.
- `accounts.default_language = NULL` → resolved app/UI language.
- Effective language = stored posting language → stored interface
  language → `Accept-Language` (q-weights, region fallback like
  `es-MX` → `es`) → `i18n.fallback_locale` (default `en`, must be a
  supported locale).
- `yonder.settings` always exposes the *stored* choices (including
  `null`) plus `effective_posting_language`.

## Consumers (later epics)

This epic owns persistence and the API; the flags take effect in:

- epic 12 — `default_visibility`, `default_language`,
  `default_sensitive`, `scheduled_deletion_days` when posting.
  Note: the API accepts both `followers_only` and `private` (the
  epic-02 CHECK admits both, matching BOOTSTRAP); their concrete
  meaning must be defined when statuses land — in Mastodon
  `private` *is* followers-only, so a decision or mapping is needed
  before the setting can drive posting.
- epic 14 — `hide_network` gates followers/following collections
  (REST + AP); `directory_opt_in` feeds the directory (gated by
  `instance.directory_enabled`).
- epic 15 — `filter_languages` on timelines.
- epic 16 — `block_stranger_notifications`, `email_notif_*`,
  `show_app_display`.
- epic 17 — `indexable` (`noindex`) and
  `include_public_posts_in_search` on search results; profile pages.

## Configuration

`i18n.fallback_locale` (`YONDER_I18N__FALLBACK_LOCALE`) — the locale
reported when neither the stored preference nor the client's
`Accept-Language` is supported. Default `en`; must be one of
`en`/`es`/`it`/`de`/`ar` (validated at startup).
