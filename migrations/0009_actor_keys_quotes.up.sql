-- Epic 06, migration 0009: actor private keys, quote authorizations,
-- inbound-processing bookkeeping and delivery-pair uniqueness.
--
-- `private_key_pem` keeps each local actor's RSA signing key next to its
-- `public_key` (migration 0002). `quote_authorizations` stores the
-- Pubby-shaped FEP-044f documents served under
-- `{actor}/quote_authorizations/{id}`.

ALTER TABLE accounts ADD COLUMN private_key_pem TEXT;

CREATE TABLE quote_authorizations (
    authorization_id TEXT PRIMARY KEY,
    data             JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Inbound dedupe rows arrive with local=false; the federation worker
-- stamps processed_at once the activity has run through Pubby.
ALTER TABLE activities ADD COLUMN processed_at TIMESTAMPTZ;

-- One ledger row per (activity, inbox): the DeliverFn fan-out seam uses
-- ON CONFLICT DO NOTHING to stay idempotent across retries.
ALTER TABLE deliveries
    ADD CONSTRAINT uq_deliveries_activity_inbox UNIQUE (activity_id, inbox_url);
