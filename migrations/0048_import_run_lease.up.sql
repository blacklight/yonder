-- Migration 0048: import run fencing token.
--
-- Each `Import` job claim rotates `run_token`; every lease-aware
-- persist/terminal write in the API-source driver is conditioned on
-- it, so a superseded run (a second job that claimed the same
-- session) can keep fetching but can never overwrite cursors,
-- pauses, or terminal state.

ALTER TABLE archive_imports ADD COLUMN run_token TEXT;
