-- 1. Edges/sync markers written before the `next`-page walk were
--    first-page-only and the hour-long claim blocked a re-sync — clear
--    the markers so the next listing read re-syncs everything.
TRUNCATE remote_network_syncs;

-- 2. A third marker direction tracks the counters refresh done by
--    `RemoteActorFetch` (it stamps `remote_network_syncs` rows but
--    never writes edges).
ALTER TABLE remote_network_syncs
    DROP CONSTRAINT remote_network_syncs_direction_check;
ALTER TABLE remote_network_syncs
    ADD CONSTRAINT remote_network_syncs_direction_check
    CHECK (direction IN ('followers', 'following', 'counters'));
