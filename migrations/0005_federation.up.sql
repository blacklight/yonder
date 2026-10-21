-- Epic 02, migration 0005: federation — activities, remote-actor cache,
-- delivery ledger and relays.
--
-- `activities` keeps the full AP payload plus the audience arrays so epic
-- 06's Postgres `ActivityPubStorage` can serve inbox/outbox and
-- addressed-to queries without re-parsing JSON.

CREATE TABLE activities (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- ActivityPub identifier URI, unique.
    activity_id  TEXT NOT NULL UNIQUE,
    type         TEXT NOT NULL,
    actor_id     TEXT NOT NULL,
    object_id    TEXT NOT NULL DEFAULT '',
    object_type  TEXT NOT NULL DEFAULT '',
    -- Full AP activity payload.
    object       JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- `to`/`cc` are reserved-ish words in SQL; suffixed instead.
    to_recipients  TEXT[] NOT NULL DEFAULT '{}',
    cc_recipients  TEXT[] NOT NULL DEFAULT '{}',
    -- to ∪ cc, the flat audience list for addressed-to queries.
    audience     TEXT[] NOT NULL DEFAULT '{}',
    local        BOOLEAN NOT NULL DEFAULT true,
    published_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_activities_object_id ON activities(object_id);
CREATE INDEX ix_activities_actor ON activities(actor_id);
-- Remote-activity retention scans (federation.remote_activity_prune_schedule).
CREATE INDEX ix_activities_type_published ON activities (type, published_at DESC);
CREATE INDEX ix_activities_audience ON activities USING gin (audience);

-- Pubby-shaped remote-actor cache (verbatim trait shape).
CREATE TABLE actor_cache (
    actor_id   TEXT PRIMARY KEY,
    actor_data JSONB NOT NULL DEFAULT '{}'::jsonb,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Durable delivery ledger: one row per (activity, remote inbox). The Redis
-- queue (epic 03) tracks the in-flight jobs; this records the state.
CREATE TABLE deliveries (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    activity_id     BIGINT NOT NULL REFERENCES activities(id) ON DELETE CASCADE,
    inbox_url       TEXT NOT NULL,
    state           TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'sent', 'failed')),
    attempts        INT NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at TIMESTAMPTZ,
    delivered_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ix_deliveries_state_next ON deliveries (state, next_attempt_at);
CREATE INDEX ix_deliveries_activity ON deliveries(activity_id);

CREATE TABLE relays (
    id                BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    inbox_url         TEXT NOT NULL UNIQUE,
    state             TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'accepted', 'rejected')),
    -- The Follow activity we sent to the relay.
    follow_activity_id TEXT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
