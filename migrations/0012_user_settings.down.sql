-- Revert epic 10 (user settings) columns.
ALTER TABLE users
    DROP COLUMN block_stranger_notifications;

ALTER TABLE accounts
    DROP COLUMN default_sensitive;
