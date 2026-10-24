-- Revert 0020: restore the 0006 domain_blocks shape, the original
-- moderation_actions CHECK, and drop the account flags.

ALTER TABLE accounts
    DROP COLUMN force_sensitive,
    DROP COLUMN disabled_at;

-- Restore the 0006 CHECK: fold `silence` back to `limit`, then drop
-- rows of the types 0006 can't represent (`none`, `sensitive`,
-- `disable`, `delete`) before re-adding the constraint.
UPDATE moderation_actions SET type = 'limit' WHERE type = 'silence';
DELETE FROM moderation_actions
    WHERE type NOT IN ('limit', 'suspend', 'unlimit', 'unsuspend',
                       'disable_2fa', 'require_password_reset',
                       'verify_email');

ALTER TABLE moderation_actions
    DROP CONSTRAINT moderation_actions_type_check,
    ADD CONSTRAINT moderation_actions_type_check CHECK (type IN (
        'limit', 'suspend', 'unlimit', 'unsuspend',
        'disable_2fa', 'require_password_reset', 'verify_email'));

ALTER TABLE domain_blocks
    RENAME COLUMN private_comment TO reason;

ALTER TABLE domain_blocks
    ADD COLUMN action TEXT;

UPDATE domain_blocks SET action = CASE
    WHEN severity = 'suspend' THEN 'suspend'
    WHEN severity = 'silence' THEN 'limit'
    WHEN reject_media THEN 'disable_media'
    WHEN reject_reports THEN 'reject_reports'
    ELSE 'limit'
END;

ALTER TABLE domain_blocks
    ALTER COLUMN action SET NOT NULL,
    ALTER COLUMN action SET DEFAULT 'suspend',
    ADD CONSTRAINT domain_blocks_action_check CHECK (action IN (
        'suspend', 'limit', 'disable_media',
        'remove_from_timeline', 'followers_only',
        'reject_reports'));

ALTER TABLE domain_blocks
    DROP CONSTRAINT domain_blocks_severity_check,
    DROP COLUMN severity,
    DROP COLUMN reject_media,
    DROP COLUMN reject_reports,
    DROP COLUMN obfuscate,
    DROP COLUMN public_comment;
