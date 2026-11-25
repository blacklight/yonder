-- 0064 is a data cleanup — the cleared `spoiler_text` values were
-- machine-generated copies of `summary` and cannot be meaningfully
-- restored. Restoring them would reintroduce the bug, so the down
-- migration is a deliberate no-op.
SELECT 1;
