-- 0063_event_dm_invites — Catodon-style event invitations over DM.
--
-- `status_event_invites` marks a `direct` status as an event invite:
-- the AP serializer emits `yonder:inviteTo`/`catodon:inviteTo` with
-- the event URI so remote Catodon/Yonder instances can render "invited
-- you to an event". The row is written inside the create transaction
-- (via `ValidatedStatus.event_invite_uri`) so the first published
-- document already carries the property.
--
-- `event_pending_dm_invites` parks DM invites whose event object has
-- not federated yet — drained post-commit when the `Event` status
-- materializes, mirroring `event_pending_invites` (0058) but without
-- the organizer-authorization check: anyone who can see an event may
-- invite people to it (Catodon semantics).

CREATE TABLE status_event_invites (
    status_id  BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    event_uri  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE event_pending_dm_invites (
    event_uri   TEXT NOT NULL,
    note_uri    TEXT NOT NULL,
    actor       TEXT NOT NULL,
    addressee   BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (event_uri, note_uri, addressee)
);

CREATE INDEX event_pending_dm_invites_event ON event_pending_dm_invites (event_uri);
