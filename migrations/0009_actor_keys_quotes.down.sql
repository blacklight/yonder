-- Epic 06, migration 0009 rollback.

ALTER TABLE deliveries
    DROP CONSTRAINT uq_deliveries_activity_inbox;

ALTER TABLE activities DROP COLUMN processed_at;

DROP TABLE quote_authorizations;

ALTER TABLE accounts DROP COLUMN private_key_pem;
