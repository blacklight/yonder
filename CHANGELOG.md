# CHANGELOG

## 1.0.10

### Added

- `events`: FEP-8a8e event support — events as first-class statuses
  (`statuses.object_type = 'Event'`) with structured side tables,
  lenient inbound parsing (Mobilizon, Gancio, WordPress Event Bridge,
  Nicolex shapes), local authoring/editing from the composer and the
  Nicolex-compatible `/api/v1/pleroma/events` adapter, a yonder-native
  RSVP API sharing the §7.1 state machine with inbound
  `Join`/`Leave`/`Accept`/`Reject`/`TentativeAccept`/`Invite`
  activities, `pleroma:*` event notifications with a reminder sweep
  and stale-remote refresh jobs, ICS export (per-event, anonymous
  profile feed, token-gated personalized subscription), and discovery
  (`/api/v1/yonder/events` with category/hashtag/online/near-me
  filters, the `/events` and `/events/discover` pages, the profile
  Events tab, and the actor `upcomingEvents` collection).
- `jobs`: Split remote-fetch work off `default` into dedicated
  `remote_status` (quote/pin/media refresh fetches), `remote`
  (actor/network/media/Webmention fetches) and `remote_bulk` (large
  remote walks) queues, and give status publishing its own `publish`
  lane — slow or broken federation remotes can no longer starve
  user-visible quote resolution, scheduled posts or interactive work.
- `jobs`: Per-queue consumer counts (`jobs.*_concurrency` overrides;
  `remote` defaults to 2× `concurrency`), an optional global
  in-flight cap for remote queues (`jobs.remote_max_in_flight`),
  periodic queue-depth logging, and Redis dedupe of identical
  remote-queue jobs so repeated read/ingest triggers can't stack
  duplicates.
- `jobs`: Source-side backpressure for remote work — a per-domain
  enqueue budget (`jobs.remote_enqueue_per_domain_per_minute`) and a
  per-domain circuit breaker
  (`jobs.remote_circuit_failure_threshold` /
  `jobs.remote_circuit_cooldown_seconds`) that pauses fetches to a
  domain after consecutive failures.
- `activitypub`: Add a C2S outbox (`POST /ap/users/{username}/outbox`)
  for Like/Announce/Follow/Create/Undo activities, plus OAuth
  discovery (RFC 8414) and dynamic client registration (RFC 7591), so
  compatible clients can publish and interact over ActivityPub
  ([`11ccece`](https://git.platypush.tech/blacklight/yonder/commit/11ccecec83d82b95356c75297b0a3c1f916824bf)).

### Fixed

- `federation`: Give user-visible status fetches their own remote-gate
  budget and circuit, and negative-cache failed signature actor
  lookups — bulk remote churn no longer starves quote/pin resolution,
  and `/ap` object requests stop paying the full remote-fetch timeout
  for unreachable actors
  ([`511946c`](https://git.platypush.tech/blacklight/yonder/commit/511946c5b71a43591b807182cc73f3eb613b2b5f)).
- Keep timelines, status pages and profiles responsive when federation
  is slow: bound inline thread-context expansion and account-lookup
  WebFinger by their real deadlines, resolve missed remote handles in
  the background via a new `remote_actor_resolve` job, and give the
  in-process worker its own database pool
  ([`a9f6893`](https://git.platypush.tech/blacklight/yonder/commit/a9f6893fb8155db6157b225f2c9ea9e6ce11a1a4)).

### Changed

- `search`: Speed up account matching — drop domain substring
  matching for exact-match plausible domains only, and back
  username/display-name autocomplete with prefix `LIKE` plus new
  btree pattern indexes (migration 0056)
  ([`7281475`](https://git.platypush.tech/blacklight/yonder/commit/7281475d0b0bc4072dbde0797b6f0f93fe2c2b)).
- `api`: Cache `/api/v1/instance` stats and `/api/v1/instance/peers`
  in Redis with a database fallback
  ([`cce6447`](https://git.platypush.tech/blacklight/yonder/commit/cce6447ed52135d7942bd4924d3a08095ec2b15c)).

## 1.0.9

### Added

- `notifications`: Add Followers and Follow Requests filters to the
  notifications page; the requests tab only appears for locked accounts
  ([`3fb843d`](https://git.platypush.tech/blacklight/yonder/commit/3fb843ddad11a7fe149eeb019928e77c6b0bd08a)).
- `cli`: Add a `remote_network_sync` job command for running remote
  follower/following syncs on demand
  ([`44a72dd`](https://git.platypush.tech/blacklight/yonder/commit/44a72dda19b02358b8cc56b62bbeda48068543b5)).

### Fixed

- `federation`: Project cached remote actor documents into `accounts`
  and stop `upsert_remote_stub` from enqueueing `RemoteActorFetch` for
  accounts that already carry an actor document — a cache hit no longer
  leaves a bare stub profile, and routine remote activity can't flood
  the default queue with redundant actor fetches
  ([`9a66dcc`](https://git.platypush.tech/blacklight/yonder/commit/9a66dcc828cc086efb221ee503c72306857a67f5)).
- `emoji`, `posts`: A stray `<` with no closing `>` in a note or post
  body no longer spins forever in the emoji and mention/hashtag
  rendering pass, which pinned a server worker thread per request
  ([`f7a1176`](https://git.platypush.tech/blacklight/yonder/commit/f7a117660b3eeebb181d0069c375f9315224bbd9)).
- `timelines`: Home-timeline fan-out runs on its own `timeline` queue
  instead of behind remote actor fetches on `default`, so new posts no
  longer appear minutes late when remote fetches back up
  ([`3109294`](https://git.platypush.tech/blacklight/yonder/commit/31092949c155097a7c3205880b22cb31a8084085)).
- `federation`: Remote actor fetches whose host doesn't resolve or
  presents an expired/mismatched/unknown-issuer TLS certificate are
  treated as gone instead of retried for the full retry budget
  ([`3109294`](https://git.platypush.tech/blacklight/yonder/commit/31092949c155097a7c3205880b22cb31a8084085)).
- `accounts`: Serve cached remote followers/following edges immediately
  and refresh them via a new `remote_network_sync` job instead of
  fetching the remote collection inline — profile network reads no
  longer block on the remote instance (previously seconds, up to ~13s)
  ([`7a6d468`](https://git.platypush.tech/blacklight/yonder/commit/7a6d468d8cd0d4fbd17a843383a1a30ca94da83a)).
- `auth`: Run argon2id password hashing/verification on tokio's blocking
  pool (`verify_password_async`/`hash_password_async`) — login,
  registration, password change/reset, account move/cancel and 2FA
  re-auth no longer occupy runtime worker threads for hundreds of ms
  per attempt
  ([`7a6d468`](https://git.platypush.tech/blacklight/yonder/commit/7a6d468d8cd0d4fbd17a843383a1a30ca94da83a)).
- `federation`: Settle outbound follows from unaddressed shared-inbox
  `Accept`/`Reject` activities — remotes that omit local `to`/`cc`
  (e.g. Flipboard) previously left requests pending forever
  ([`a3913bb`](https://git.platypush.tech/blacklight/yonder/commit/a3913bb4f5489511735eea7f91d7f2507141d0da)).
- `federation`: Settle outbound follows when `Accept`/`Reject` reference
  the `Follow` by id instead of embedding it (e.g. tags.pub) — the
  reference is resolved against locally published activities only
  ([`3f79f80`](https://git.platypush.tech/blacklight/yonder/commit/3f79f80637ae4977273b6fa88d30ba3b505e6553)).
- `deps`: Upgraded pubby-sdk to 0.6.19 (RFC 9421 `@target-uri`
  signatures, standalone `CryptographicKey` keyIds, non-canonical PEM)
  ([`3f79f80`](https://git.platypush.tech/blacklight/yonder/commit/3f79f80637ae4977273b6fa88d30ba3b505e6553)).
- `activitypub`: Route `POST /inbox` to the shared inbox handler and
  include the `Follow` activity id in `Undo` activities, so unfollows
  reference the original follow
  ([`a98898e`](https://git.platypush.tech/blacklight/yonder/commit/a98898ebdc1304993385496658a28cdcaae27d0a)).
- `compose`: Seed replies with the parent's content warning and track
  it in the draft, preserving user edits across reseeds
  ([`5ac80d7`](https://git.platypush.tech/blacklight/yonder/commit/5ac80d7799853bfdc3750d6aec00465b556dbd54)).
- `polls`: Ingest remote poll votes from `Create` activities and
  self-heal tallies — votes are summed from option tallies, local
  votes survive remote re-ingest, and ballots no longer emit
  notifications
  ([`4573083`](https://git.platypush.tech/blacklight/yonder/commit/45730830cfb57fa6a9940a2458dbe90bda46257c),
  [`c15e092`](https://git.platypush.tech/blacklight/yonder/commit/c15e09291e6f1c8a87b70b3f97be1c585f2473ac),
  [`f458253`](https://git.platypush.tech/blacklight/yonder/commit/f4582533597a996bf0ba02f805228e9f144d60c3),
  [`67f8f2a`](https://git.platypush.tech/blacklight/yonder/commit/67f8f2a59c38270250e5cab53f62d1126063931b)).
- `jobs`: Notification fan-out runs on its own `notifications` queue
  instead of sharing `default` with remote actor and network fetches,
  so a backlog of slow remote jobs no longer delays notifications by
  hours
  ([`e624076`](https://git.platypush.tech/blacklight/yonder/commit/e624076d7adb70803515df9734b755104125dacd)).
- `notifications`: Notifications linked to a status are now deleted
  when the status is deleted — local and remote deletes, unreblogs and
  webmention retractions no longer leave recipients with notifications
  pointing at a gone post
  ([`b4f4986`](https://git.platypush.tech/blacklight/yonder/commit/b4f49867219ee610f89927dc48401e4de185be29)).

## 1.0.8

### Fixed

- `federation`: Run synchronous pubby calls (`dispatch_multi`,
  `process_inbox_activity`, actor/object fetches, deliveries) on tokio's
  blocking pool via `spawn_blocking` instead of `block_in_place` — enough
  concurrent blocking remote fetches could occupy every runtime worker
  thread and freeze `serve`/`worker` until restart
- `search`: Scope the `indexable = false` opt-out to search-engine
  indexing surfaces only — it no longer hides the account or its
  statuses from the instance's own search; account discovery is
  governed by `discoverable` alone and status search by
  `include_public_posts_in_search`, matching Mastodon's `noindex`
  semantics

## 1.0.7

### Added

- `docs`: Document the domain-split configuration — `instance.domain`
  is the acct/WebFinger identity (`@user@domain`) while
  `federation.base_url` is the origin actor/object IRIs are minted
  under; `config.toml.example` and the config field docs now say so
  ([`9fc1c56`](https://git.platypush.tech/blacklight/yonder/commit/9fc1c56ef4b9067c9bc5ba304bfa9b577013bea4)).
- `federation.signature_scheme` (`"draft-cavage"` default, or `"rfc9421"`)
  selects the wire format for outbound signed requests — deliveries and
  object/actor fetches. Inbound requests are verified in either format
  ([`d4de0da`](https://git.platypush.tech/blacklight/yonder/commit/d4de0da2e19ead53449cc84aa6cb5b51c62f7d8a)).
- Per-actor Ed25519 keys: each local account and the instance actor get an
  Ed25519 key (migration 0053, generated on first load for existing rows).
  It is advertised in `additionalPublicKeys` and signs RFC 9421 deliveries
  and object fetches when `federation.signature_scheme = "rfc9421"`
  ([`d4de0da`](https://git.platypush.tech/blacklight/yonder/commit/d4de0da2e19ead53449cc84aa6cb5b51c62f7d8a)).
- `statuses`: Add `local` visibility — posts marked `local` are readable
  only by accounts logged on this instance (never anonymous or remote
  viewers) and never federate; replies and quotes of `local` posts stay
  instance-scoped, and the composer, default-visibility setting and
  status cards expose the new audience
  ([`4577f22`](https://git.platypush.tech/blacklight/yonder/commit/4577f22be8f81d073c3215ef0ee0fe209ae56a62)).
- Honor `indexable = false` (the profile search-engine opt-out) on
  every indexing surface: profile, follow-list, status and embed pages
  carry `<meta name="robots" content="noindex">` — injected into the
  served HTML and set client-side for JS-rendering crawlers — public
  profile/hashtag feeds drop the author's items, status text search
  hides their posts (still findable by the author), and outgoing
  webmention source documents get the tag
  ([`60d140d`](https://git.platypush.tech/blacklight/yonder/commit/60d140d2e76e0e1ef8c75a3954ee56c200004c42)).

### Fixed

- `instance`: `urls.streaming_api` on the v1/v2 instance entities is
  now derived from `federation.base_url` (scheme remapped to
  `ws`/`wss`) instead of `instance.domain` — domain-split deployments
  advertise the host that actually serves `/api/v1/streaming`, and
  non-default ports/schemes are expressed correctly
  ([`9fc1c56`](https://git.platypush.tech/blacklight/yonder/commit/9fc1c56ef4b9067c9bc5ba304bfa9b577013bea4)).

## 1.0.6

### Fixed

- `blocks`: Enforce per-user account and domain blocks across reads —
  blocked or domain-blocked accounts are now hidden from status views,
  timelines, interaction lists, follow listings, notifications,
  conversations and ActivityPub object serving, and their inbound
  Like/Announce/EmojiReact activities no longer apply
  ([`95aeef7`](https://git.platypush.tech/blacklight/yonder/commit/95aeef77a88c6eebb7d2cb12a352bef596153fa4)).

## 1.0.5

### Added

- `instance`: Add `instance.single_user` personal-instance mode — visitors
  without a working session who open `/` are redirected to the configured
  local profile (`/@user`); signed-in users keep the normal home timeline.
  Exposed to the SPA as `yonder.single_user` on the instance entities
  ([`9f049a0`](https://git.platypush.tech/blacklight/yonder/commit/9f049a0d3b07bbb60647c7f2ea2f57ea96271d99)).

### Fixed

- `math`: Fix posts containing math delimiters and non-ASCII text
  panicking — the failure also wedged retries on a stale idempotency
  reservation, now treated as abandoned after 15 minutes
  ([`b6097fa`](https://git.platypush.tech/blacklight/yonder/commit/b6097fa48dad896a748db7e67a861b8e72066c14)).
- `http`: Emit absolute URLs in pagination `Link` headers — clients like
  Phanpy (masto.js) parse link targets with `new URL()` and rejected
  Yonder's relative paths, failing to load timelines
  ([`cd06edb`](https://git.platypush.tech/blacklight/yonder/commit/cd06edbf2533f0cf675c82fe54d5620190d97b27)).
- `math`: Use paper-style numbered links (`(1)`, `(2)`, …) as the
  fallback for rendered formulas instead of the raw LaTeX link text, and
  apply the formula styles globally so quotes, embeds and conversations
  render — and hide — them consistently
  ([`44d3293`](https://git.platypush.tech/blacklight/yonder/commit/44d32934f5eeb8570e71170a753aa5b318287666)).

### Changed

- `docs`: Update the table of contents in `README.md` and `INSTALL.md`
  ([`f18d79d`](https://git.platypush.tech/blacklight/yonder/commit/f18d79d60f3e266a331e253d2a3d8a83b808e903)).

## 1.0.4

### Added

- `markdown`: Render inline `![alt](url)` images in posts and federate them
  as ActivityPub image attachments
  ([`599cda5`](https://git.platypush.tech/blacklight/yonder/commit/599cda5e79feda652fb69d7ae2fdb213551bd4b7)).
- `math`: Render LaTeX formulas (`$…$`, `$$…$$`, `\(…\)`, `\[…\]`) to inline
  images through a configurable external renderer command — real LaTeX
  (tectonic/pdflatex), MathJax or KaTeX all work via the stdin→PNG contract.
  Formulas become real media attachments that federate as standard images
  to clients without formula support, with a linked LaTeX-source fallback
  for clients that strip `<img>`; formula rows are hidden from the
  on-screen media gallery since they already render inline. Disabled by
  default — see `[math]` in `config.toml.example`
  ([`b2c871f`](https://git.platypush.tech/blacklight/yonder/commit/b2c871ff02dc7d7abee6d7adb884d8ac0bf8a277)).

### Fixed

- `timelines`: Keep local statuses visible when domain blocks are present
  ([`aa01f58`](https://git.platypush.tech/blacklight/yonder/commit/aa01f5846a7ff76a1b8650a9b67b48db9b91e7d5)).
- `status`: Keep media and link previews behind content warnings
  ([`e5f82b5`](https://git.platypush.tech/blacklight/yonder/commit/e5f82b51fd696adda47172a6d06a083a5d36edb9)).
- `previews`: Reject media URLs as embed players
  ([`3136813`](https://git.platypush.tech/blacklight/yonder/commit/31368135d3907c7280c80ab710abd053c4fca2c8)).
- `ap`: Treat type-only `Image`, `Video`, and `Audio` ActivityPub attachments
  as media kinds, preserving inline images from SocialHome/PyFed
  ([`994f2c3`](https://git.platypush.tech/blacklight/yonder/commit/994f2c3be8a9d9bc8f5436656745d90f12077c3e)).
- `math`: Fix multi-line `$$…$$` formulas rendering as literal LaTeX or
  corrupted single lines — formulas are now extracted from the raw source
  before Markdown/plain-text rendering, preserving `aligned` environments,
  `\\` row separators and backslash spacing commands verbatim.

### Changed

- `docs`: Move the installation guide to `INSTALL.md`
  ([`5d358c1`](https://git.platypush.tech/blacklight/yonder/commit/5d358c1456cd0ab1d35bb5fe58f18d59f88649b5)).

## 1.0.3

### Added

- `layout`: Add a Compose shortcut to the middle of the mobile bottom
  navigation bar, replacing the duplicate profile link
  ([`f9e0e70`](https://git.platypush.tech/blacklight/yonder/commit/f9e0e7013fe28f67684f91541d0cff5ee3455ac1),
  [`d704211`](https://git.platypush.tech/blacklight/yonder/commit/d7042112c9021bc15fc497a8edfea181d57d8ca6)).
- `ci`: Build release artifacts once per tag and publish a GitHub release
  ([`681b00e`](https://git.platypush.tech/blacklight/yonder/commit/681b00e59d10b27098f9f29e5577742582ee835c)).

### Fixed

- `theme`: Persist the theme toggle via the settings store
  ([`c57a12d`](https://git.platypush.tech/blacklight/yonder/commit/c57a12d5a047babc36d7ba0816b43ca815c9f9cf)).
- `oauth`: Accept a trailing slash on `/oauth/authorize`
  ([`3e25778`](https://git.platypush.tech/blacklight/yonder/commit/3e257789d51b377c7abf662fc0283194a19f1fd7)).

### Changed

- `docs`: Add an architecture overview and link it from the README
  ([`81f9f88`](https://git.platypush.tech/blacklight/yonder/commit/81f9f887c67d35c97fbfbddaa37122d2d320b966)).

## 1.0.1

### Added

- Publish `quoteAuthorization` when accepting a `QuoteRequest`
  ([`d924240`](https://git.platypush.tech/blacklight/yonder/commit/d9242404276dc876fbf3d9e98d2d27926d63e746)).

### Fixed

- `compose`: Show an "Updated" toast with an Open link after edits
  ([`cdb29eb`](https://git.platypush.tech/blacklight/yonder/commit/cdb29eba11785e5a8a546292a0fcd1b2995ed301)).
- `auth`: Sync settings on restore to apply compose defaults
  ([`6c94cd7`](https://git.platypush.tech/blacklight/yonder/commit/6c94cd720ede0a9e5956b5a3d025644c5d2316f9)).

## 1.0.0

First public release of Yonder! 🎉
