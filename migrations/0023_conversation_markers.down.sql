-- Revert epic 25 (migration 0023): conversation markers and stored keys.
DROP TABLE conversation_markers;
DROP INDEX ix_statuses_conversation_key;
ALTER TABLE statuses
    DROP COLUMN conversation_key;
