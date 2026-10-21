ALTER TABLE config_overlays DROP COLUMN updated_by;

DROP TABLE sessions;
DROP TABLE oauth_refresh_tokens;
DROP TABLE oauth_access_tokens;
DROP TABLE oauth_authorizations;
DROP TABLE oauth_applications;
DROP TABLE backup_codes;
DROP TABLE webauthn_credentials;
DROP TABLE two_factor;
DROP TABLE account_aliases;
DROP TABLE users;
DROP TABLE accounts;
