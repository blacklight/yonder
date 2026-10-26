-- `RemoteActorFetch` stamps `remote_network_syncs` rows on failure so
-- profile reads stop re-enqueueing a job on every view of a dead
-- actor: 'actor' = last fetch attempt failed (suppresses re-enqueue
-- for the actor-refresh window), 'actor_gone' = the remote answered
-- 404/410 or the URL is unfetchable (suppresses for the longer gone
-- recheck window).
ALTER TABLE remote_network_syncs
    DROP CONSTRAINT remote_network_syncs_direction_check;
ALTER TABLE remote_network_syncs
    ADD CONSTRAINT remote_network_syncs_direction_check
    CHECK (direction IN ('followers', 'following', 'counters', 'actor', 'actor_gone'));
