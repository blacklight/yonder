-- Revert epic 23 (frontend foundation) per-user frontend column.
ALTER TABLE users
    DROP COLUMN frontend;
