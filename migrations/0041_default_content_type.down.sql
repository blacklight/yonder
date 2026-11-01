-- Drop the per-user default post content type column.

ALTER TABLE accounts
    DROP COLUMN default_content_type;
