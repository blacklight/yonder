-- Revert 0054: restore the pre-`local` visibility CHECK sets. Fails
-- while `local` rows exist — delete or rewrite them first.

ALTER TABLE historical_observations
    DROP CONSTRAINT historical_observations_visibility_check,
    ADD CONSTRAINT historical_observations_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'followers_only',
                              'private', 'direct'));

ALTER TABLE accounts
    DROP CONSTRAINT accounts_default_visibility_check,
    ADD CONSTRAINT accounts_default_visibility_check
        CHECK (default_visibility IN ('public', 'unlisted',
                                      'followers_only', 'private',
                                      'direct'));

ALTER TABLE statuses
    DROP CONSTRAINT statuses_visibility_check,
    ADD CONSTRAINT statuses_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'followers_only',
                              'private', 'direct'));
