# yonder

![yonder](branding/banner.png)

[![Build Status](https://ci-cd.platypush.tech/api/badges/blacklight/yonder/status.svg)](https://ci-cd.platypush.tech/blacklight/yonder)
[![Codacy Badge](https://app.codacy.com/project/badge/Coverage/02b6f6c4ec79460f847018963e4bbc00)](https://app.codacy.com/gh/blacklight/yonder/dashboard)
[![Codacy Badge](https://app.codacy.com/project/badge/Grade/02b6f6c4ec79460f847018963e4bbc00)](https://app.codacy.com/gh/blacklight/yonder/dashboard)
[![Github stars](https://img.shields.io/github/stars/blacklight/yonder?style=flat&logo=Github)](https://github.com/blacklight/yonder)
[![Github forks](https://img.shields.io/github/forks/blacklight/yonder?style=flat&logo=Github)](https://github.com/blacklight/yonder)
[![Last Commit](https://img.shields.io/github/last-commit/BlackLight/yonder.svg)](https://git.platypush.tech/yonder/yonder/commits/branch/main)
[![License](https://img.shields.io/github/license/blacklight/yonder.svg)](https://git.platypush.tech/blacklight/yonder/src/branch/main/LICENSE)

<!-- toc -->

- [🌟 Overview](#overview)
- [⚡ Features](#features)
  * [📣 Posting & timelines](#posting-timelines)
  * [📢 Social & federated](#social-federated)
  * [🧳 Migration & portability](#migration-portability)
  * [⚖️ Moderation & administration](#moderation-administration)
  * [🛠️ Platform](#platform)
- [📦 Installation](#installation)
- [⚙️ Configuration](#configuration)
- [⚡ Running the service](#running-the-service)
  * [Creating a user](#creating-a-user)
  * [Web push notifications](#web-push-notifications)
- [▶️ Testing the installation](#testing-the-installation)
- [🧩 Integrations](#integrations)
- [API](#api)
- [🛠️ Development](#development)
- [📜 License](#license)

<!-- tocstop -->

## 🌟 Overview

Yonder is a self-hosted, Mastodon-compatible
[ActivityPub](https://activitypub.rocks/) server written in Rust — a whole
social network in a single binary.

It speaks the Mastodon client API, so the apps you already use on your phone
and desktop just work when pointed at your instance. It speaks ActivityPub,
so your community federates with Mastodon, Akkoma, Pleroma and the rest of
the Fediverse. And it speaks migration: moving an account *to* or *from*
another server — followers, posts, media and all — is a first-class feature,
not an afterthought.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://s3.fabiomanganiello.com/fabio/screenshots/yonder/desktop-home.png">
  <img src="https://s3.fabiomanganiello.com/fabio/screenshots/yonder/light-desktop-home.png" alt="The Yonder home timeline on desktop">
</picture>

## ⚡ Features

### 📣 Posting & timelines

- 📝 **Rich posts** — plain text or Markdown, content warnings, polls,
  media attachments with editable alt text, scheduled posts, post editing
  and saved drafts
- 💬 **Quotes & reactions** — quote posts with author authorization and
  custom emoji reactions, on top of the usual replies, boosts and likes
- 🏠 **Every timeline Mastodon has** — home, local and federated feeds,
  hashtag timelines, and user-defined lists — with real-time updates over
  WebSocket streaming
- #️⃣ **Hashtags that work for you** — follow tags into your home feed,
  trending tags, and server-side filters (`warn`/`hide`/`blur`) that work
  across every timeline
- 🧵 **Threads that complete themselves** — reply contexts are fetched on
  demand, and remote like/boost counts are resolved where the origin
  exposes them

### 📢 Social & federated

- 🌐 **Full ActivityPub federation** via
  [pubby-sdk](https://crates.io/crates/pubby-sdk) — interoperates with
  Mastodon, Akkoma, Pleroma, Misskey and other Yonder instances
- 🔌 **Mastodon-compatible API** (v1 and v2) — point Tusky, Ivory,
  Elk, Phanpy or any Mastodon client at your instance and log in
- ✉️ **Private conversations** — direct messages as conversations, not
  just posts with a lock icon
- 🔔 **Notifications your way** — real-time in-app notifications, Web
  Push, and email, with per-type preferences
- 😀 **Custom emoji** — per-instance emoji and emoji packs, rendered
  consistently in posts, profiles and reactions
- 🛜 **Feeds & embeds** — RSS 2.0 and Atom feeds for profiles, tags and
  lists; oEmbed/embeddable status cards; Webmentions in both directions

### 🧳 Migration & portability

- 🚚 **Move in either direction** — signed ActivityPub `Move` handling,
  follower migration with retries and progress tracking, and moved-account
  banners, compatible with Mastodon and Akkoma
- 📦 **Your data is yours** — Mastodon-compatible export archives
  (posts, media, lists, bookmarks, follows, blocks…) with signed download
  links — and suspended accounts still get export-only access
- 📥 **Import from anywhere** — pull your archive, or connect a remote
  Mastodon/Akkoma account over OAuth and import posts, bookmarks,
  favourites, follows and more — preserving timestamps and provenance
- 🔐 **Identity that survives** — profile fields, link verification and
  moved-account markers all travel with you

### ⚖️ Moderation & administration

- 🛡️ **User-side tools** — mutes, blocks, reports (optionally forwarded
  to the remote instance via `Flag`), and server-side content filters
- 🏛️ **Instance-side tools** — user limiting and suspension, domain
  blocks, federation relays, and an admin panel for users, emoji, settings
  and audit logs
- 🚪 **Every registration policy** — open, invite-only, approval-required
  or closed, plus invite management
- 🌍 **Truly international** — the bundled web UI ships in five languages
  including full RTL support, with light and dark themes

### 🛠️ Platform

- 🦀 **One Rust binary** — no Ruby, no Elixir, no sprawling dependency
  tree; PostgreSQL + Redis are the only services
- 🔑 **OAuth2 provider** — authorization-code + PKCE, device flow,
  password and client-credentials grants, plus scoped, revocable API
  tokens for your own scripts
- 🖥️ **Pluggable frontends** — the bundled web UI is just the default;
  alternative frontends can be installed alongside it
- 📖 **Open API** — OpenAPI 3 spec at `/openapi.json` with bundled
  Swagger UI at `/swagger-ui`
- 🔒 **Safety by default** — argon2id passwords, SSRF-guarded remote
  fetches, Redis-backed rate limits, and durable at-least-once
  federation delivery

## 📦 Installation

See [installation page](./INSTALL.md).

## ⚙️ Configuration

A single `config.toml` controls the instance — see
[`config.toml.example`](config.toml.example) for every documented key.
Configuration resolves in this order (highest wins):

1. **Database overlays** (`config_overlays` table, editable from the
   admin panel)
2. **`YONDER_*` environment variables** — `__` is the nesting
   separator, e.g. `YONDER_INSTANCE__NAME` for `instance.name`
3. **`config.toml`**
4. **Built-in defaults**

Run `yonder config list` to see every key in both `config.toml` and
environment-variable form. For split-domain deployments — handles on
one domain, ActivityPub on another — see
[`docs/instance.md`](docs/instance.md#domains-the-acct-domain-vs-the-federation-origin).

## ⚡ Running the service

```bash
yonder serve     # HTTP API + web UI (+ in-process jobs when jobs.enabled)
yonder worker    # the background job queues (production deployment)
```

### Creating a user

```bash
# Docker
docker compose run --rm yonder user create --username alice --admin

# Cargo/binary install
yonder user create --username alice --admin
```

See [`docs/cli.md`](docs/cli.md) for the full command reference —
users, domain blocks, relays, emoji, frontend installs, config
overlays, migrations and ad-hoc job execution.

### Web push notifications

Before exposing the instance to browsers, generate the Web Push VAPID
keys once and add them to `.env` (or `config.toml`):

```bash
docker compose run --rm yonder webpush generate-keys
# or: yonder webpush generate-keys
```

## ▶️ Testing the installation

```bash
curl https://your.instance/healthz
curl https://your.instance/api/v1/instance
```

Then open the instance in a browser, sign in, and check that your
profile resolves remotely:

```bash
curl 'https://mastodon.social/.well-known/webfinger?resource=acct:you@your.instance'
```

## 🧩 Integrations

- **Mastodon clients** — any app that speaks the Mastodon API
  (Tusky, Ivory, Elk, Phanpy, Mona, …) works against `/api/v1` and
  `/api/v2`
- **Feed readers** — profiles, tags and lists expose RSS 2.0 and Atom
  feeds with `<link rel="alternate">` discovery
- **OAuth2 apps** — register third-party applications via the standard
  `/api/v1/apps` + `/oauth` flow, or issue scoped API tokens from the
  settings UI
- **Blog & web** — public posts emit Webmentions and answer incoming
  ones; statuses can be embedded on external pages via `embed.js`

## API

A documented REST API under `/api/v1` and `/api/v2` — Mastodon-shaped
where Mastodon defines a contract, namespaced under `yonder.*` where it
doesn't. The OpenAPI 3 spec is served at `/openapi.json` and a bundled
Swagger UI lives at `/swagger-ui`.

The streaming WebSocket at `/api/v1/streaming` implements Mastodon's
`subscribe`/`unsubscribe` protocol — `user`, `public`, `public:local`,
`hashtag` and `list` streams.

## 🛠️ Development

Yonder is a Rust workspace (`axum`, `sqlx`, `tokio`) plus a Vue 3 +
TypeScript SPA in `frontend/`.

The code structure and the main components of the service are described
in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). Developer
documentation lives in [`docs/development.md`](docs/development.md) —
environment setup, migrations, the test harness, job-queue and HTTP
internals, and the release pipeline. Feature-by-feature API contracts
live alongside it in [`docs/`](docs/).

## 📜 License

Yonder is free software, released under the
[AGPL-3.0 license](LICENSE).
