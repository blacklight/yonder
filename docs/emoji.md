# Custom emoji

Mastodon-compatible custom emoji: an admin-managed catalogue,
`:shortcode:` rendering in statuses and profiles, ActivityPub
emission/recording, and bulk import.

## Public API

`GET /api/v1/custom_emojis` — anonymous. Enabled, `visible_in_picker`
local emoji as `CustomEmoji` entities (`{shortcode, url, static_url,
visible_in_picker, category}`). `static_url` equals `url` until
animated variants land.

## Rendering

Substitution is **read-time** (`src/emoji/render.rs`): stored
`content_html` keeps the literal `:code:` text, and
`posts::entity::status_entity`, account assembly (`note`, `fields[]`),
and feed item content all run
`emoji::render::substitute` when building entities — so emoji uploaded
*after* a post was written still render, and disabled/renamed emoji
stop resolving immediately. `display_name` stays plain text like
Mastodon — shortcodes in it appear in the account's `emojis` array
and clients render them themselves.

- The map is **domain-scoped**: remote statuses/accounts resolve
  `:code:` against the author's domain (recorded remote emoji), never
  a same-named local emoji.
- The pass only rewrites text runs — `a`, `code`, `pre`, `script`,
  `style` subtrees are skipped (same rule as the mention/hashtag
  enhancement), so links and code blocks are untouched.
- Unknown or disabled shortcodes stay as literal text.
- The `emojis` array on Status and Account entities is derived from
  the same map (shortcode presence in the rendered text).

## Admin API

All under `/api/v1/admin/`, gated by `require_scope(…, "admin")`
(token `admin` scope + `accounts.role = 'admin'`):

| Route | Purpose |
|---|---|
| `POST /api/v1/admin/emojis` | multipart `file` + `shortcode` (+`pack_id`, `category`) |
| `GET /api/v1/admin/emojis` | list (`?pack_id=`, `?include_disabled=`) |
| `GET/PATCH/DELETE /api/v1/admin/emojis/{id}` | show / rename, recategorize, repack, toggle / delete (+ stored image) |
| `POST /api/v1/admin/emojis/import_zip` | multipart zip of images; `{imported, skipped, results[]}` per-entry |
| `POST /api/v1/admin/emojis/import_dir` | `{from_dir, pack_id?}` — flat scan under `admin.emoji_import_dir` |

PATCH `category`/`pack_id` are tri-state: omit the field to keep the
value, send `null` to clear it, send a value to set it. `category`
accepts at most 100 characters.

`import_zip` with `overwrite=true` replaces an existing shortcode in
place: the swap keeps the row id (and reaction references), resets
`category`/`pack_id`/`disabled`/`visible_in_picker` to defaults,
validates and stores the new image *before* touching the row, and
deletes the old object only after the swap commits — a bad file
leaves the old emoji intact.
| `GET/POST /api/v1/admin/emoji_packs` | list (with counts) / create `{name}` |
| `PATCH/DELETE /api/v1/admin/emoji_packs/{id}` | rename / delete (emoji keep rows, `pack_id` cleared) |

Shortcodes validate as `[a-zA-Z0-9_]{2,100}` (the schema CHECK, one
char stricter than the DB's `+` bound); the filename stem supplies the
shortcode on imports, sanitized to lowercase `a-z0-9_`.

### Safety limits

- Per-file cap `media.max_emoji_size` (default 256 KiB), image decode
  through the shared `MediaKind::Emoji` pipeline (no blurhash).
- Zip imports: entry count ≤ 500, cumulative uncompressed ≤
  `media.max_emoji_zip_uncompressed` (50 MiB), per-entry ratio ≤ 100:1,
  entry paths flat-or-one-deep — `..`, absolute (`/`, `\`) and deeper
  names are refused; non-image entries are reported as skipped.
- `import_dir` requires `admin.emoji_import_dir`; the requested path
  must canonicalize inside it and per-file symlinks can't escape.
  At most 500 files per call; each file is size-checked before it is
  read, so an oversize file never lands whole in memory.
- Emoji store under `emoji/{uploader}/{uuid}.{ext}` — instance-owned;
  account deletion never sweeps `emoji/` keys.

## Federation

Outbound posts emit `{"type":"Emoji","name":":code:","icon":{…}}` tags
for every `:code:` in the text (local emoji only; actor-level `Emoji`
tags need pubby `ActorConfig.tag` support — deferred). Inbound `Emoji`
tags on remote Notes *and* actor documents upsert `custom_emojis` rows
with `domain` + `remote_url` (`src/posts/remote.rs`,
`src/activitypub/storage.rs`), which is what lets remote-domain
rendering resolve their codes. Remote URLs must be http(s).

## Reaction integration

`interactions::reactions` already resolves `:code:` against this same
table (`lookup_custom_emoji`) — uploads made here are immediately
reactable.
