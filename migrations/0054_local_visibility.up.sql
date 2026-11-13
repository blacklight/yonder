-- Migration 0054: `local` status visibility.
--
-- `local` posts are readable by any authenticated account on this
-- instance and never federate (Songhive's `Visibility.LOCAL`
-- semantics). The stored value joins the existing CHECK sets on
-- `statuses`, `accounts.default_visibility` and
-- `historical_observations` (import provenance mirrors status
-- visibility verbatim).

ALTER TABLE statuses
    DROP CONSTRAINT statuses_visibility_check,
    ADD CONSTRAINT statuses_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'followers_only',
                              'private', 'direct', 'local'));

ALTER TABLE accounts
    DROP CONSTRAINT accounts_default_visibility_check,
    ADD CONSTRAINT accounts_default_visibility_check
        CHECK (default_visibility IN ('public', 'unlisted',
                                      'followers_only', 'private',
                                      'direct', 'local'));

ALTER TABLE historical_observations
    DROP CONSTRAINT historical_observations_visibility_check,
    ADD CONSTRAINT historical_observations_visibility_check
        CHECK (visibility IN ('public', 'unlisted', 'followers_only',
                              'private', 'direct', 'local'));
