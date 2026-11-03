-- Migration 0049: import run heartbeat.
--
-- `updated_at` is bumped by the `Import` claim and every lease-fenced
-- progress write, so a `running` session with no scheduled retry and
-- no heartbeat for a while is a dead run the sweep can reclaim.

ALTER TABLE archive_imports
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
