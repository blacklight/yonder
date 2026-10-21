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
