# Yonder frontend

Vue 3 + Vite + TypeScript SPA for the Yonder ActivityPub server.
Decoupled from the backend: it builds to a static bundle
(`frontend/dist`) that the backend serves at `/` as the built-in
`yonder` frontend, and instances can install alternative frontends
alongside it (Pleroma/Akkoma-style) via the admin API.

## Requirements

- Node.js ≥ 24 (vite 8 / vitest 5 / jsdom 30 toolchain)
- A running Yonder backend on `localhost:3000` for dev (not needed
  for `api:gen`, which uses `cargo run -- openapi`). If the backend
  binds a different host/port, set `VITE_BACKEND_URL` in a gitignored
  `.env.local` (e.g. `VITE_BACKEND_URL=http://10.0.0.34:3000`).

## Scripts

- `npm run dev` — dev server; proxies `/api` (incl. the streaming
  websocket), `/oauth`, `/media`, `/.well-known`, `/nodeinfo`,
  `/openapi.json`, `/swagger-ui` to `$VITE_BACKEND_URL` (default
  `http://localhost:3000`). `/users`, `/tags`, `/lists` and `/ap`
  are intentionally not proxied — they would shadow SPA routes.
- `npm run build` — `vue-tsc` + `vite build` → `dist/`
- `npm run lint` / `npm run format` / `npm run type-check`
- `npm run test` — Vitest (jsdom + @vue/test-utils)
- `npm run api:gen` — regenerate `src/api/types.ts` from the
  backend's OpenAPI document via `yonder openapi` (no running
  server needed); re-run and commit after API changes
- `npm run i18n:check` — report UI strings missing from `en.json`
- `npm run sync-branding` — copy icons/favicon/logo from `../branding`

## Layout

- `src/api` — typed client (auth, `{ error }` mapping, `Link`
  pagination) + generated OpenAPI types
- `src/components/ui` — design-system components (Button, Input,
  Card, Avatar, Modal, Menu, Tooltip); `src/components/layout`;
  `src/components/admin` — shared admin controls (SettingField,
  SettingHelp, SettingsSection, AccountActions, ConfirmDialog)
- `src/pages/admin` — the admin panel (`/admin/*`, `meta.requiresRole:
"admin"`); `src/pages/moderation` — the report queue
  (`/moderation*`, admin+moderator). Admin settings pages render the
  `/api/v1/yonder/admin/settings` registry via `stores/adminSettings`;
  each field shows TOML key/env/source, Reset on overlays, and
  `requires_restart` badges
- `src/stores` — auth, theme, settings; `src/composables`
- `src/styles` — tokens (light/dark/accent), base, rtl
- `src/i18n` — `vue-i18n` with `en` eager, `es`/`it`/`de`/`ar` lazy;
  `ar` renders RTL via postcss-rtlcss
- `examples/minimal` — template/fixture alternative frontend

## Custom-frontend contract

An alternative frontend installed via the admin API must:

- contain an `index.html` at its directory root (a single wrapper
  directory is hoisted on install) and use relative asset paths;
- consume `/api` and `/openapi.json` from the same origin, honor
  `Link`-header pagination and the `yonder` extension fields;
- include a `frontend.json` manifest (`{ "name", "version"?,
"source"?, "entry"? }` — `entry` defaults to `index.html`);
- be served statically — no SSR;
- honor `x-yonder-frontend` for service-worker lifecycle: the
  built-in SW unregisters when the served frontend is not `yonder`, so
  a custom frontend with its own SW must implement the same check.

Per-user frontend choice is stored via `PATCH /api/v1/yonder/settings`
(`frontend` key) in this epic; serving a per-user frontend lands with
the login flow in epics 24/25.

## Core SPA (epic 24)

- **Auth**: the sign-in form accepts an access token (`SignInPage`);
  OAuth login lands in epic 25. The token persists in `localStorage`
  and re-verifies on reload; a `401` clears it. Routes carry
  `meta.requiresAuth` to redirect anonymous visitors.
- **Timelines**: `/`, `/home`, `/local`, `/explore`, `/tags/:name`,
  `/lists/:id` share `TimelinePage` + the `timeline` Pinia store
  (Link-header pagination, pending-buffer for live updates).
- **Live updates**: `stores/streaming` is the sole raw-WebSocket
  consumer — a singleton `/api/v1/streaming` socket with
  reference-counted subscriptions and exponential-backoff reconnect.
  The token rides in the WS query string (`?access_token=`, required
  for Mastodon compatibility) — make sure the production proxy does
  not log query strings on the streaming path.
- **Compose**: dedicated `/compose` route (chosen over an inline
  modal) with media, alt text, polls, CW, language, visibility,
  scheduling and a per-submission `Idempotency-Key`.
- **Sanitization**: status HTML is re-sanitized client-side with
  DOMPurify (allowlist mirrors the server's ammonia policy).
- **Favicon privacy**: remote accounts show `https://<domain>/favicon.ico`
  loaded directly — the request reveals the viewer's IP to that
  domain. Documented trade-off; revisit if a proxy endpoint lands.
