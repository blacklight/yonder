-- Migration 0050: deferred boost intents.
--
-- An imported Announce whose target isn't materialized locally parks
-- here instead of wrapping an empty stub row: the boost is written
-- when the remote object lands (`backfill_pending_boosts` runs on
-- every status-materialization path), so permanently unreachable
-- targets never surface as empty boost cards. `published`/`visibility`
--/`original_id` provenance is kept verbatim for the eventual row.

CREATE TABLE pending_boosts (
    id           BIGINT PRIMARY KEY DEFAULT timestamp_id('pending_boosts'),
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_uri   TEXT NOT NULL,
    announce_id  TEXT NOT NULL,
    published    TIMESTAMPTZ NOT NULL,
    visibility   TEXT NOT NULL,
    import_id    BIGINT NOT NULL REFERENCES archive_imports(id) ON DELETE CASCADE,
    federate     BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, target_uri, announce_id)
);

CREATE INDEX ix_pending_boosts_target ON pending_boosts (target_uri);
