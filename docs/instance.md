# Instance metadata & the `/about` page

Public instance information and the SPA's `/about` surface — all
anonymous-eligible.

## Endpoints

- `GET /api/v1/instance` — Mastodon's v1 shape: `uri`, `title`,
  `description`, `short_description`, `email`, `version`,
  `urls.streaming_api`, `stats` (`user_count`, `status_count`,
  `domain_count`), scalar `registrations`/`approval_required`/
  `invites_enabled`, `configuration`, `contact_account`, `rules`,
  `thumbnail` (URL string, null when unset), plus the Akkoma-style
  `pleroma` capability block.
- `GET /api/v2/instance` — the v2 shape: `domain` instead of `uri`,
  `source_url`, `registrations` as an object, `usage.users.active_month`
  (local authors with a status in the last 30 days), `thumbnail` as a
  Mastodon-4.x `{url, blurhash, versions}` object, `icon` as a
  `[{src, size}]` list, `contact` `{email, account, name, url}`.
- `GET /api/v1/instance/extended_description` — the long-form
  `instance.extended_description` text; 404 when unset (Mastodon
  semantics).
- `GET /api/v1/instance/peers` — known remote domains, gated by
  `instance.peers_api_enabled` (404 when off).
- `GET /api/v1/instance/domain_blocks` — public moderated-servers
  listing (`domain`, `severity`, `comment`), gated by
  `instance.expose_moderated_instances` (404 when off). The SPA hides
  the section on a 404.

Both entities also carry a `yonder` extension block; today it only
has `single_user` (see below).

## Single-user redirect

`instance.single_user` puts the instance in personal-instance mode:
when it names a valid local username, visitors **without a working
session** who open `/` are redirected to that profile
(`/@<user>`) instead of the public landing feed. Signed-in users keep
the normal home timeline, and only the root route is affected — every
other public page (`/explore`, `/tags/…`, `/about`, permalinks) still
works anonymously.

The value must pass the same username rules as registration
(`[a-zA-Z0-9_]`, length ≤ `content.max_username_length`, not reserved);
blank/whitespace clears the setting. It is admin-writable through the
settings overlay like any other `instance.*` key, and it is exposed to
the SPA as `yonder.single_user` (`null` when unset) so the redirect
decision is made client-side — page loads carry no bearer token, so
the server cannot distinguish a signed-in browser from an anonymous
one.

## `rules` and `contact`

`instance.rules` is a string list in config but serializes as
Mastodon Rule entities (`{"id": "1", "text": "…"}`) in both versions —
that is the shape registration forms and the `/about` page number.

`instance.contact_url` pointing at a local profile
(`{base}/@user` or `{base}/users/{user}` on this instance's own host)
resolves to a full Account entity for `contact.account` (v2) /
`contact_account` (v1). Remote or unparseable values leave it `null`;
the v2 `contact.name`/`contact.url`/`contact.email` extensions still
let the page render an "administered by" block.

## Branding assets

`instance.logo_path` and `instance.banner_path` are filesystem paths
(e.g. `/var/lib/yonder/logo.png`), not media-store keys. They are
served at `GET /api/v1/instance/logo` and `GET /api/v1/instance/banner`
— read off disk per request so an overlay path change or a replaced
file takes effect without a restart. Only known image extensions get
an image MIME (png/jpg/jpeg/webp/gif/svg/avif/ico; anything else is
`application/octet-stream`), SVGs are sandboxed with
`Content-Security-Policy: default-src 'none'`, and responses cache for
5 minutes (`public, max-age=300` — the URL is stable while the file
beneath it may change, so never `immutable`). Unset or unreadable
paths 404, and the entities only advertise asset URLs that resolve.

## The `/about` page

`frontend/src/pages/AboutPage.vue` follows Mastodon's layout:

- **Hero** — `thumbnail.url` banner, `icon[0].src` logo, title +
  domain, `short_description`/`description` tagline, and a
  sign-up/sign-in CTA shown to anonymous visitors while
  `registrations.enabled` (closed-registration instances keep the
  sign-in link only).
- **Stats band** — `usage.users.active_month` followed by the v1
  `stats` counters.
- **About this server** — `extended_description` when configured,
  falling back to the short description. Both pass through
  `sanitizeHtml` before rendering.
- **Server rules** — numbered list from `rules`.
- **Administered by** — the resolved `contact.account` rendered as a
  profile link with avatar, else the configured
  `contact.name`/`contact.url`/`contact.email`.
- **Moderated servers** — the public `domain_blocks` listing with
  localized severity labels (`noop`/`silence`/`suspend`), hidden when
  the endpoint 404s.
- **Footer** — `version`, `languages` and the `source_url` link.
