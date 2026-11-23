-- 0058_event_pending_invites — invites that arrive before their event.
--
-- A bare-URI `Invite{object: Event}` can reach us before the event
-- object itself federates. The invitee rows can't be written until
-- `events` has a row, so the invite's addressing is parked here and
-- drained by `events::remote::drain_pending_invites` when the event
-- materializes (any ingest path — inbox Create, fetch, embedded).

CREATE TABLE event_pending_invites (
    id                     BIGSERIAL PRIMARY KEY,
    event_uri              TEXT NOT NULL,
    invite_activity_uri    TEXT NOT NULL,
    actor                  TEXT NOT NULL,
    addressee              TEXT NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_uri, invite_activity_uri, addressee)
);
CREATE INDEX event_pending_invites_uri_idx ON event_pending_invites (event_uri);
