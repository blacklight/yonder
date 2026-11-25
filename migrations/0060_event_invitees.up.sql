-- 0060_event_invitees — authoring-side invitee list on `events`.
--
-- `direct`-visibility events carry their invitee account ids here so
-- edits can re-derive the `status_mentions` rows (which the mention
-- re-sync deletes wholesale) and Section 16's Invite fan-out has an
-- authoritative list to address. Invitees also exist as
-- `event_participations` rows (`state = 'invited'`) once the RSVP
-- flow runs; this column is the author's declared set, not the
-- acceptance state.

ALTER TABLE events
    ADD COLUMN invitees BIGINT[] NOT NULL DEFAULT '{}';
