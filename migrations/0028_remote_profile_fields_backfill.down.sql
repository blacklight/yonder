-- Projection-only repair: there is no pre-backfill state to restore
-- (the column was always '[]' for remote rows); leave values as they
-- are — a re-fetch or the refresh sweep keeps them current.
SELECT 1;
