DROP INDEX ix_oauth_access_tokens_app_user;

ALTER TABLE sessions
    DROP COLUMN access_token_id;

ALTER TABLE oauth_access_tokens
    DROP COLUMN name;
