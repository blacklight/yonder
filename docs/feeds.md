# RSS & Atom feeds

Yonder serves RSS 2.0 and Atom 1.0 feeds for profiles, hashtags and
lists. Feeds are generated server-side with quick-xml (all dynamic text
and attribute values are escaped, and XML-invalid control characters are
dropped), bounded by `content.max_feed_items`
(default 50, validated to `1..=200`), and rate-limited on the `feed`
bucket (default 30 requests / 60 s).

## Endpoints

| Feed | URL | Visibility | Auth |
|---|---|---|---|
| Profile | `GET /users/{username}.atom` `.rss` | Local accounts only; **public** posts | None |
| Hashtag | `GET /tags/{tag}.atom` `.rss` | **public** posts carrying the tag (case-insensitive) | None |
| List | `GET /lists/{id}.atom` `.rss` | List-owner scope: members' `public`, `unlisted`, `followers_only`, `private` (never `direct`) | Required — see below |

Content types are `application/atom+xml; charset=utf-8` and
`application/rss+xml; charset=utf-8`. Usernames and tags are matched
case-insensitively; a tag feed uses the lowercase tag in its title and
links. Suspended/deleted accounts, unknown usernames, unknown or missing
extensions (`.json`, none, `foo.atom.xml`), and
multi-segment paths all return 404. Boosts are excluded from profile and
list feeds (a boost row has no content of its own); hashtag feeds never
match them because boost rows carry no tags. Replies are included —
a feed is "latest public posts", not the web UI's "posts" tab.

## Feed contents

Each item/entry carries: `id` (the status URI), `link` (`url`, else
`uri`), `title` (`spoiler_text`, else a plain-text preview truncated to
100 characters), `published` (`created_at`), `updated` (`edited_at`,
else `created_at`), `author` (`display_name`, else `@acct`), and the
sanitized post HTML (`content_html`, produced by the render pipeline) in
`content type="html"` (Atom) / `description` (RSS).

## List-feed authentication

List feeds require a token because lists are private:

- `Authorization: Bearer <token>` — preferred.
- `?access_token=<token>` — for feed readers that cannot set headers.

The token needs the `read:lists` scope. Anonymous requests get 401;
foreign or nonexistent lists get 404 (indistinguishable by design).

> **Caveat:** `?access_token=` puts a long-lived credential in a URL —
> feed readers and any URL logging can retain it, and the feed exposes
> `followers_only` content. Prefer the header where the reader allows
> it, and rotate the token if a feed URL leaks. The query token is not
> logged by the server (`TraceLayer` records only the request path).

## Auto-discovery

`feeds::discovery_links(base_path, title)` emits the
`<link rel="alternate" type="application/atom+xml" …>` and
`application/rss+xml` tags for a feed base path (e.g. `/users/alice`),
with attribute values XML-escaped. The frontend embeds them into `<head>` when
profile/hashtag pages exist — no HTML pages are served yet, so the helper is
the contract. List feeds are never advertised.

## Implementation notes

- Feed reads reuse the shared timeline query path
  (`timelines::query::fetch` + `TimelineScope::{Account, Tag, List}`).
  Profile/hashtag feeds set the `Filters::public_only` flag
  (`s.visibility = 'public'`); the anonymous account tab's
  public+unlisted is deliberately stricter here.
- Feeds are a single bounded `LIMIT max_feed_items` query — no
  pagination cursors. `paginate::parse` is not used: its 1..=40 clamp
  would silently cap the configured limit.
- Authors are resolved in one `account_mapper::load_many` query over
  the page's distinct account ids.
