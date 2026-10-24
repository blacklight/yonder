-- Epic 21, migration 0020: moderation severity model + account flags.
--
-- Reshapes `domain_blocks` to Mastodon's model: a single `severity`
-- enum (`noop`/`silence`/`suspend`) plus independent `reject_media` /
-- `reject_reports` booleans and `private_comment` / `public_comment` /
-- `obfuscate` presentation fields. The old `action` values fold in:
--   suspend                        → severity = 'suspend'
--   limit / remove_from_timeline / followers_only → severity = 'silence'
--   disable_media                  → severity = 'noop' + reject_media
--   reject_reports                 → severity = 'noop' + reject_reports
-- `reason` becomes `private_comment` (admin-only notes; the public
-- listing uses `public_comment`).
--
-- `moderation_actions.type` gains the Mastodon action vocabulary
-- (`none`, `sensitive`, `disable`, `silence`) plus Yonder's `delete` —
-- `limit`/`suspend` and the reversal verbs stay for compatibility.
--
-- `accounts` gains the flags user actions flip: `force_sensitive`
-- (`sensitive` action — all new media marked sensitive) and
-- `disabled_at` (`disable` action — frozen account: login/token use
-- rejected, content intact, reversible).

ALTER TABLE domain_blocks
    ADD COLUMN severity TEXT,
    ADD COLUMN reject_media BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN reject_reports BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN obfuscate BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN public_comment TEXT NOT NULL DEFAULT '';

UPDATE domain_blocks SET
    severity = CASE action
        WHEN 'suspend' THEN 'suspend'
        WHEN 'reject_reports' THEN 'noop'
        WHEN 'disable_media' THEN 'noop'
        ELSE 'silence'
    END,
    reject_media = (action = 'disable_media'),
    reject_reports = (action = 'reject_reports');

ALTER TABLE domain_blocks
    ALTER COLUMN severity SET NOT NULL,
    ALTER COLUMN severity SET DEFAULT 'silence',
    ADD CONSTRAINT domain_blocks_severity_check
        CHECK (severity IN ('noop', 'silence', 'suspend'));

ALTER TABLE domain_blocks
    DROP CONSTRAINT domain_blocks_action_check,
    DROP COLUMN action;

ALTER TABLE domain_blocks
    RENAME COLUMN reason TO private_comment;

ALTER TABLE moderation_actions
    DROP CONSTRAINT moderation_actions_type_check,
    ADD CONSTRAINT moderation_actions_type_check CHECK (type IN (
        'none', 'sensitive', 'disable', 'silence', 'suspend', 'delete',
        'limit', 'unlimit', 'unsuspend',
        'disable_2fa', 'require_password_reset', 'verify_email'));

ALTER TABLE accounts
    ADD COLUMN force_sensitive BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN disabled_at TIMESTAMPTZ;
