-- Best-effort cache of remote actors' followers/following collections.
-- Remote→remote edges are not real `follows` rows (those drive local
-- counters and delivery), so they live here: read at REST time by the
-- `accounts/{id}/followers|following` endpoints, refreshed from the
-- actor's remote AP collections.

-- One row per (account, direction) — the sync timestamp doubles as the
-- claim marker: the UPSERT in `social::follows::ensure_remote_network`
-- only re-stamps when the row is stale, so concurrent readers can't
-- stampede the same remote fetch, and a failed/empty fetch is itself
-- throttled instead of retried on every request.
CREATE TABLE remote_network_syncs (
    account_id  BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    direction   TEXT NOT NULL CHECK (direction IN ('followers', 'following')),
    synced_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, direction)
);

-- Members of a remote account's network listing, in remote-collection
-- order. `position` is the pagination cursor; entries past the remote
-- page hold local `follows` edges not covered by the remote list (e.g.
-- a local follower the remote collection omitted), appended at sync.
CREATE TABLE remote_network_edges (
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    direction         TEXT NOT NULL CHECK (direction IN ('followers', 'following')),
    member_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    position          INT NOT NULL,
    PRIMARY KEY (account_id, direction, member_account_id)
);
CREATE INDEX ix_remote_network_edges_page
    ON remote_network_edges (account_id, direction, position);
