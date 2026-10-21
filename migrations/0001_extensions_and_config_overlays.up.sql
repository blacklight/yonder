-- Epic 02, migration 0001: search extensions and the admin config-overlay
-- table. The overlay contract (dotted TOML path -> JSONB value) is consumed
-- by `db::config_overlays::fetch` and the config loader from epic 01.

-- Trigram indexes back the ILIKE-based search on accounts/statuses
-- (epic 17). Requires privileges to install extensions; on managed
-- Postgres the operator may need to allowlist `pg_trgm` first.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE config_overlays (
    key        TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
