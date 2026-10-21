-- Epic 02, migration 0006: moderation — instance domain blocks, reports,
-- moderation actions and the audit log.

-- Instance-level moderation of remote domains (BOOTSTRAP "Instance
-- moderation"): suspend/defederate, disable media, federated timeline
-- removal, followers-only, reject reports.
CREATE TABLE domain_blocks (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    domain               TEXT NOT NULL UNIQUE,
    action               TEXT NOT NULL DEFAULT 'suspend' CHECK (action IN (
                             'suspend', 'limit', 'disable_media',
                             'remove_from_timeline', 'followers_only',
                             'reject_reports')),
    reason               TEXT NOT NULL DEFAULT '' CHECK (char_length(reason) <= 2000),
    created_by_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE reports (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id         BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_account_id  BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id          BIGINT REFERENCES statuses(id) ON DELETE SET NULL,
    category           TEXT NOT NULL DEFAULT 'other' CHECK (category IN ('spam', 'violation', 'legal', 'other')),
    comment            TEXT NOT NULL DEFAULT '' CHECK (char_length(comment) <= 8000),
    forward            BOOLEAN NOT NULL DEFAULT false,
    action_taken_at    TIMESTAMPTZ,
    assigned_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_reports_target ON reports(target_account_id);
-- Open-reports queue for the moderation panel.
CREATE INDEX ix_reports_open ON reports (created_at DESC) WHERE action_taken_at IS NULL;

CREATE TABLE moderation_actions (
    id                   BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id           BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    type                 TEXT NOT NULL CHECK (type IN (
                             'limit', 'suspend', 'unlimit', 'unsuspend',
                             'disable_2fa', 'require_password_reset',
                             'verify_email')),
    reason               TEXT NOT NULL DEFAULT '' CHECK (char_length(reason) <= 2000),
    created_by_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    expires_at           TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_moderation_actions_account ON moderation_actions (account_id, created_at DESC);

-- Audit trail of admin/moderator actions (BOOTSTRAP "Moderation audit log").
CREATE TABLE audit_log (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor_account_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    action          TEXT NOT NULL,
    target_type     TEXT NOT NULL DEFAULT '',
    target_id       BIGINT,
    details         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_audit_log_created ON audit_log (created_at DESC);
