DELETE FROM remote_network_syncs WHERE direction = 'counters';
ALTER TABLE remote_network_syncs
    DROP CONSTRAINT remote_network_syncs_direction_check;
ALTER TABLE remote_network_syncs
    ADD CONSTRAINT remote_network_syncs_direction_check
    CHECK (direction IN ('followers', 'following'));
