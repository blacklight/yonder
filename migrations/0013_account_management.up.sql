-- Epic 11, migration 0013: account lifecycle — move bookkeeping,
-- tombstoned deletions, invite attribution, archive import/export
-- progress and post-import dedupe.
--
-- Deleted accounts keep a redacted `accounts` row (username, actor_id
-- and keys stay): in-flight `Delete` deliveries still sign with the
-- stored key and the username/actor URL can never be re-registered.

ALTER TABLE accounts
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD COLUMN moved_at   TIMESTAMPTZ;

-- Which invite (if any) a user signed up with.
ALTER TABLE users
    ADD COLUMN invite_id BIGINT REFERENCES invites(id) ON DELETE SET NULL;

-- Import dedupe: one imported copy per source object per account.
CREATE UNIQUE INDEX uix_statuses_account_original
    ON statuses (account_id, original_id) WHERE original_id IS NOT NULL;

-- Scheduled/pending account deletions, one row per account.
CREATE TABLE account_deletions (
    id                      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id              BIGINT NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
    -- NULL = self-service; else the admin/mod who requested it.
    requested_by_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    scheduled_at            TIMESTAMPTZ NOT NULL,
    -- Whether the account was already suspended when deletion was
    -- scheduled — cancellation then must NOT lift the suspension.
    was_suspended           BOOLEAN NOT NULL DEFAULT false,
    state                   TEXT NOT NULL DEFAULT 'pending'
                            CHECK (state IN ('pending', 'running', 'cancelled', 'completed')),
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_account_deletions_due ON account_deletions (scheduled_at) WHERE state = 'pending';

CREATE TABLE archive_exports (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- 'archive' (full zip) or one of the per-kind CSV/JSON exports.
    kind         TEXT NOT NULL,
    state        TEXT NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'running', 'done', 'failed', 'expired')),
    file_key     TEXT,
    file_size    BIGINT,
    error        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    expires_at   TIMESTAMPTZ
);
CREATE INDEX ix_archive_exports_account ON archive_exports (account_id, created_at DESC);

CREATE TABLE archive_imports (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    mode         TEXT NOT NULL DEFAULT 'merge' CHECK (mode IN ('merge', 'overwrite')),
    state        TEXT NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'running', 'done', 'failed')),
    file_key     TEXT NOT NULL,
    -- Resume cursor + progress (rows processed / total, failures).
    cursor       BIGINT NOT NULL DEFAULT 0,
    total        INT,
    processed    INT NOT NULL DEFAULT 0,
    failed       INT NOT NULL DEFAULT 0,
    error        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);
CREATE INDEX ix_archive_imports_account ON archive_imports (account_id, created_at DESC);
