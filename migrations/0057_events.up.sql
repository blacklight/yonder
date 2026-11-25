-- 0057_events — FEP-8a8e Event support.
--
-- Events are statuses (`statuses.object_type = 'Event'`, name in
-- `statuses.title`, description in `content`/`content_html`) plus an
-- `events` side table — the same pattern polls and articles use, so
-- replies, boosts, favourites, reports, timelines and search keep
-- working unchanged.
--
-- `events.start_time`/`end_time` are instants (timestamptz): the
-- authoritative value is what federates. Display uses `timezone`
-- (IANA id) or `utc_offset_seconds` when no zone is known. `end_time`
-- is always populated after ingest — a missing or inverted remote
-- `endTime` is inferred as start + `events.default_duration` with
-- `end_time_inferred` set and `display_end_time` forced false, and the
-- inferred end is never displayed.
--
-- `status` is the FEP event status; `start_time` may be NULL only for
-- `postponed` events.
--
-- `join_mode` and the participation tables back RSVP: local events
-- track every participant (local or remote), remote events track only
-- local accounts' own participation (remote attendee lists are never
-- mirrored).

CREATE TABLE events (
    status_id                 BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    start_time                TIMESTAMPTZ,
    end_time                  TIMESTAMPTZ,
    end_time_inferred         BOOLEAN NOT NULL DEFAULT false,
    display_end_time          BOOLEAN NOT NULL DEFAULT true,
    timezone                  TEXT,
    utc_offset_seconds        INT,
    status                    TEXT NOT NULL DEFAULT 'scheduled' CHECK (status IN
                                ('scheduled','tentative','cancelled','postponed','rescheduled','moved_online')),
    previous_start_time       TIMESTAMPTZ,
    join_mode                 TEXT NOT NULL DEFAULT 'free' CHECK (join_mode IN
                                ('free','restricted','external','invite','none')),
    external_participation_url TEXT CHECK (char_length(external_participation_url) <= 2048),
    maximum_attendee_capacity INT CHECK (maximum_attendee_capacity >= 0),
    participants_count        INT NOT NULL DEFAULT 0,
    pending_count             INT NOT NULL DEFAULT 0,
    remote_participants_count INT,
    categories                TEXT[] NOT NULL DEFAULT '{}',
    banner_media_id           BIGINT REFERENCES media_attachments(id) ON DELETE SET NULL,
    flyer_media_id            BIGINT REFERENCES media_attachments(id) ON DELETE SET NULL,
    -- Comments/replies hint from the origin (Mobilizon
    -- `commentsEnabled`/`repliesModerationOption`) — UI hint only.
    comments_enabled          BOOLEAN NOT NULL DEFAULT true,
    organizers_disclosed      BOOLEAN NOT NULL DEFAULT true,
    attendees_visibility      TEXT NOT NULL DEFAULT 'organizers'
                                CHECK (attendees_visibility IN ('organizers','participants','public')),
    attendees_uri             TEXT,
    remote_join_protocol      TEXT CHECK (remote_join_protocol IN ('join','invite_response','external','none')),
    raw_status                TEXT,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (start_time IS NOT NULL OR status = 'postponed'),
    CHECK (end_time IS NULL OR start_time IS NULL OR end_time > start_time)
);
CREATE INDEX events_start_idx ON events (start_time) WHERE start_time IS NOT NULL;
CREATE INDEX events_end_idx   ON events (end_time);
CREATE INDEX events_categories_idx ON events USING gin (categories);

CREATE TABLE event_locations (
    id            BIGSERIAL PRIMARY KEY,
    status_id     BIGINT NOT NULL REFERENCES events(status_id) ON DELETE CASCADE,
    position      SMALLINT NOT NULL,
    kind          TEXT NOT NULL CHECK (kind IN ('place','virtual')),
    uri           TEXT,
    name          TEXT CHECK (char_length(name) <= 500),
    url           TEXT CHECK (char_length(url) <= 2048),
    address_text  TEXT CHECK (char_length(address_text) <= 1000),
    street_address TEXT, locality TEXT, region TEXT, postal_code TEXT, country TEXT,
    latitude      DOUBLE PRECISION CHECK (latitude BETWEEN -90 AND 90),
    longitude     DOUBLE PRECISION CHECK (longitude BETWEEN -180 AND 180),
    UNIQUE (status_id, position)
);
CREATE INDEX event_locations_geo_idx ON event_locations (latitude, longitude)
    WHERE latitude IS NOT NULL AND longitude IS NOT NULL;

CREATE TABLE event_participations (
    id                     BIGSERIAL PRIMARY KEY,
    status_id              BIGINT NOT NULL REFERENCES events(status_id) ON DELETE CASCADE,
    account_id             BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    role                   TEXT NOT NULL DEFAULT 'participant' CHECK (role IN ('organizer','participant')),
    state                  TEXT NOT NULL CHECK (state IN
                             ('pending','accepted','rejected','tentative','declined','ignored','invited')),
    protocol               TEXT NOT NULL CHECK (protocol IN ('join','invite_response','local')),
    request_activity_uri   TEXT UNIQUE,
    response_activity_uri  TEXT,
    invite_activity_uri    TEXT,
    participation_message  TEXT CHECK (char_length(participation_message) <= 2000),
    reminded_at            TIMESTAMPTZ,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (status_id, account_id)
);
CREATE INDEX event_participations_account_idx ON event_participations (account_id, state);

CREATE TABLE event_organizers (
    status_id   BIGINT NOT NULL REFERENCES events(status_id) ON DELETE CASCADE,
    position    SMALLINT NOT NULL,
    account_id  BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    uri         TEXT,
    name        TEXT,
    kind        TEXT NOT NULL CHECK (kind IN ('actor','link','person','organization')),
    PRIMARY KEY (status_id, position)
);
