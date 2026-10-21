# yonder

An ActivityPub server in Rust, (almost) fully compatible with Mastodon

## Development setup

Prerequisites: a Rust toolchain (`cargo`, `rustfmt`, `clippy` — see
`rust-toolchain.toml`), PostgreSQL and Redis for runtime services, Python 3
for the Markdown TOC hook, and [`pre-commit`](https://pre-commit.com) for the
git hooks.

```bash
cp config.toml.example config.toml   # adjust for your environment
cp .env.example .env                 # secrets and env overrides
pre-commit install

cargo build
cargo test
```

Configuration resolves in this order (highest wins): database overlays
(`config_overlays` table) → `YONDER_*` environment variables → `config.toml`
→ built-in defaults. Environment variables use `__` as the nesting
separator, e.g. `YONDER_INSTANCE__NAME` for `instance.name`. Run
`yonder config list` to see every key in both forms.

## Database

The PostgreSQL schema is embedded in the binary as reversible sqlx
migrations (`migrations/*.up.sql` + `*.down.sql`). Migrations run
automatically on `yonder serve` (disable with `server.migrate_on_start =
false`), or can be applied explicitly:

```bash
yonder migrate              # apply all pending migrations (idempotent)
yonder migrate --revert     # roll back the most recent migration
yonder migrate --revert 3   # roll back to version 3 (exclusive)
```

Migrations are additive and forward-only; destructive changes live in
clearly-named migrations. The DB-backed integration tests spin up a
Postgres container via testcontainers and are skipped when no container
runtime is reachable.

## Redis & background jobs

Redis provides the cache layer, the durable job queue and the cron
scheduler. Everything is namespaced under `redis.key_prefix` (`yonder:`
by default).

- **Cache** — `cache::RedisCache` (`{prefix}:cache:` keys): `get`/`set`
  with TTL, `delete`/`delete_pattern` (SCAN-based), and single-flight
  `get_or_load` request coalescing. `cache::Redis` also exposes the raw
  `redis::Client` so Pubby's `RedisDocumentStore` can share the
  connection for ActivityPub document caching (epic 06).
- **Queue** — one Redis stream per queue: `default` (interactive work),
  `federation` (latency-sensitive delivery) and `bulk` (high-volume
  fan-out). Jobs persist across restarts; delivery is at-least-once, so
  **handlers must be idempotent**. Failures retry with exponential
  backoff (`jobs.backoff_*`) up to `jobs.max_attempts`, then land on
  `{prefix}:queue:{name}:dead`. Delayed jobs are parked in a sorted set
  until due; `XAUTOCLAIM` reclaims work abandoned by dead workers.
- **Cron** — cleanup schedules are read from
  `federation.remote_activity_prune_schedule` and
  `media.remote_cleanup_schedule` (5-field crontab syntax) and fire by
  enqueuing the matching cleanup job kind.

Run the worker standalone (`yonder worker`, the production deployment)
or in-process with `serve` (`jobs.enabled = true`, the dev default).
`docker-compose.yml` wires a `worker` service — it's a stub until
epic 28 ships the image.

### Adding a job kind

1. Add the variant to `jobs::JobKind` and route it to a `Queue` in
   `JobKind::queue()` — pick `federation` only for latency-sensitive
   delivery, `bulk` for high-volume fan-out.
2. Implement `jobs::JobHandler` (`async fn run(&self, ctx, payload)`)
   and `registry.register(kind, handler)` at startup, replacing the
   stub registered by `jobs::stub::register_all`.
3. Enqueue with `dispatcher.enqueue(kind, payload)` or
   `enqueue_delayed(kind, payload, delay)`. Payloads are `serde_json`
   values; keep them small and self-contained.

## HTTP API

`yonder serve` binds `server.host`/`server.port` and, when
`jobs.enabled` is set, runs the job worker in-process alongside it.
The router (`http::build_router`) is modular: feature epics merge
their subrouters under `http::api::v1`/`v2` and register an OpenAPI
fragment in their module's `ApiDoc` — no changes to core wiring.

- **Endpoints** — `GET /healthz`, `GET /openapi.json` (OpenAPI 3 via
  utoipa), Swagger-UI at `/swagger-ui`, and the streaming WebSocket at
  `/api/v1/streaming`. `/ap` ActivityPub routes are nested in epic 06.
- **Middleware** — request ids (`x-request-id`, generated when absent
  and propagated to the response), per-request tracing spans, CORS
  (`http.cors_enabled` + `http.cors_origins`, off by default), and
  Redis-backed rate limits.
- **Auth** — handlers take `AuthContext` (401 on missing/invalid
  token) or `MaybeAuth` (optional). Tokens resolve from
  `Authorization: Bearer` or `?access_token=` against
  `oauth_access_tokens` (`token_hash = hex(sha256(token))`; epic 07
  owns issuance). The streaming endpoint shares the same resolution.
- **Rate limiting** — `ratelimit::layer(&state, "<bucket>")` applied
  with `.route_layer()`. Buckets are the `rate_limit.*` config keys
  (`search`, `timeline`, `account_creation`, `relation`, `status`,
  `authentication`); authenticated requests use
  `authenticated_limit` when set and a separate `u:{id}` identity.
  Rejects with 429 + `Retry-After`.
- **Errors** — `Error` implements `IntoResponse` with Mastodon's
  `{"error": "…"}` shape; 5xx responses return a generic message
  (internals are logged, never leaked).
- **Streaming** — Mastodon's protocol: `{"type":"subscribe",
  "stream":"<name>"}` / `unsubscribe` frames, server→client
  `{"stream":[…],"event":…,"payload":…}` frames, 30 s server pings
  and echoed client pings. `user*` streams require auth and only
  receive the owner's events (`user:{id}`). Producers publish via
  `state.bus.publish(stream, event, payload)` (epic 16).

### Adding a route module

1. Create `src/http/api/{v1,v2}/<feature>.rs` (or a dedicated module);
   handlers take `State<Arc<AppState>>` plus extractors.
2. Merge its `Router<Arc<AppState>>` in `api::v1::router()` /
   `api::v2::router()`.
3. Annotate handlers with `#[utoipa::path]` and list them in the
   module's `ApiDoc` `paths(...)` — `openapi::document()` merges it.
4. Attach `ratelimit::layer(&state, "<bucket>")` where the endpoint
   maps to a configured bucket.
