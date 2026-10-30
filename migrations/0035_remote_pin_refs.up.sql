-- Remote featured collections: item URIs that haven't materialized
-- yet park here; `backfill_pending_pins` converts them to
-- `status_pins` rows when the object lands (the `quote_uri` /
-- `backfill_pending_quotes` pattern). `idx` preserves collection
-- order for the `?pinned=true` list.
CREATE TABLE remote_pin_refs (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    object_uri TEXT NOT NULL,
    idx        INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, object_uri)
);
CREATE INDEX remote_pin_refs_uri ON remote_pin_refs (object_uri);
