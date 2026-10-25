-- Epic 25, migration 0023: direct-message conversations.
--
-- `statuses.conversation_key` — SHA-256 (lowercase hex) of the sorted,
-- deduplicated participant ids (author + `status_mentions.account_id`s)
-- joined with `,`. Stored so conversations group and thread without a
-- per-request participant-set scan. Populated only for direct statuses;
-- tombstones keep their key (queries exclude them) so a thread can fall
-- back to the newest remaining visible status. The column is
-- maintained by `posts::conversations::refresh_conversation_key` at
-- every `status_mentions` write site; this backfill must produce the
-- identical value — same numeric ordering, same `,` join, UTF-8 input
-- to `sha256`.
--
-- `conversation_markers` — per-caller read/hide markers. Markers are
-- always written for the authenticated caller only.
-- `hidden_through_status_id` implements Mastodon's caller-only
-- conversation deletion: the conversation disappears from the list
-- until a status newer than the marker revives it for that caller.

ALTER TABLE statuses
    ADD COLUMN conversation_key TEXT;

UPDATE statuses s
SET conversation_key = (
    SELECT encode(
               sha256(convert_to(
                   string_agg(p.account_id::text, ',' ORDER BY p.account_id),
                   'UTF8')),
               'hex')
    FROM (
        SELECT s.account_id
        UNION
        SELECT m.account_id
        FROM status_mentions m
        WHERE m.status_id = s.id
    ) p
)
WHERE s.visibility = 'direct';

-- Per-key scans order by newest status; the partial predicate keeps
-- the index to the DM rows the conversation queries touch.
CREATE INDEX ix_statuses_conversation_key
    ON statuses (conversation_key, id DESC)
    WHERE visibility = 'direct';

CREATE TABLE conversation_markers (
    account_id               BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    conversation_key         TEXT NOT NULL,
    last_read_status_id      BIGINT,
    hidden_through_status_id BIGINT,
    UNIQUE (account_id, conversation_key)
);
