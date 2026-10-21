-- Revert 0008: restore case-sensitive unique constraints.

DROP INDEX ix_mutes_target;

DROP INDEX uix_tags_name;
ALTER TABLE tags ADD CONSTRAINT tags_name_key UNIQUE (name);

DROP INDEX uix_users_email;
ALTER TABLE users ADD CONSTRAINT users_email_key UNIQUE (email);

DROP INDEX uix_accounts_username_domain;
CREATE UNIQUE INDEX uix_accounts_username_domain
    ON accounts (username, COALESCE(domain, ''));
