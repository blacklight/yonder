-- Migration 0042: remote thread expansion state.
--
-- `in_reply_to_uri` keeps the remote parent's AP URI when the parent
-- row isn't stored yet — it powers relink-on-arrival (a late parent
-- connects its already-stored replies without a fetch) and gives the
-- ancestor walk a climb start that doesn't refetch the reply.
--
-- `remote_*_count`/`remote_counts_fetched_at` hold like/boost/quote
-- totals fetched from the origin at expansion time. They are rendered
-- as GREATEST(local, remote) — never merged into the transactional
-- local counters.
--
-- `last_selected_at` records when a remote status was last read
-- through /context — the recency predicate the context-only pruning
-- eligibility uses (batched, at most one write per status per hour).
-- `fetched_at` is the *insertion* time — remote `created_at` is the
-- origin's `published`, which can be years old for freshly fetched
-- thread context, so pruning must age rows from when they were
-- stored.
--
-- `status_expansions` tracks when each remote status's ancestor walk
-- and subtree expansion last completed, driving the grace-period rules
-- that keep repeated /context reads from re-fetching.

ALTER TABLE statuses ADD COLUMN in_reply_to_uri TEXT;
CREATE INDEX ix_statuses_in_reply_to_uri
    ON statuses (in_reply_to_uri) WHERE in_reply_to_id IS NULL;

ALTER TABLE statuses
    ADD COLUMN remote_likes_count INT,
    ADD COLUMN remote_shares_count INT,
    ADD COLUMN remote_quotes_count INT,
    ADD COLUMN remote_counts_fetched_at TIMESTAMPTZ,
    ADD COLUMN last_selected_at TIMESTAMPTZ,
    ADD COLUMN fetched_at TIMESTAMPTZ NOT NULL DEFAULT now();

CREATE TABLE status_expansions (
    status_id        BIGINT PRIMARY KEY REFERENCES statuses (id) ON DELETE CASCADE,
    path_done_at     TIMESTAMPTZ,
    subtree_done_at  TIMESTAMPTZ,
    depth_reached    INT NOT NULL DEFAULT 0
);
