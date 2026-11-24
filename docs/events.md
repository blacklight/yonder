# Events

FEP-8a8e event support: structured events as first-class statuses,
local authoring, RSVPs, notifications, calendar exports and discovery.

An event is a status with `object_type = 'Event'` — the title lives in
`statuses.title`, the description in the normal content fields, and
everything event-specific in the `events` side table plus
`event_locations` / `event_organizers` / `event_participations`. The
Mastodon-compatible status entity carries the metadata under
`status.yonder.event`.

## Config

```toml
[events]
enabled = true                     # false blocks new event creation and hides the SPA's event surfaces
default_duration_minutes = 180     # inferred endTime for open-ended events
max_locations = 5
max_categories = 3
allow_custom_categories = false    # authoring restricted to the FEP category list
summary_excerpt_chars = 300        # generated AP summary excerpt
reminder_lead_minutes = 60         # pleroma:event_reminder lead time
remote_refresh_after_minutes = 360 # staleness before a participant view re-fetches a remote event
reminder_sweep_schedule = "*/5 * * * *"  # empty disables reminders
map_provider = "osm"                  # "google" links places to Google Maps instead

[events.geocoding]
provider = "none"                  # "nominatim" | "google" enables place lookup
nominatim_url = "https://nominatim.openstreetmap.org"
user_agent = ""                    # required for nominatim (their usage policy)
google_api_key = ""                # required for provider = "google"
```

`events.enabled = false` only gates authoring and the frontend
surfaces — inbound Event parsing and existing events keep working.

## Authoring

- `POST /api/v1/statuses` with `post_type=event` and an `event`
  object (title, start/end times, timezone, locations, categories,
  join mode, invitees, banner/flyer media ids). The composer's event
  builder produces it; the Nicolex-compatible adapter
  (`/api/v1/pleroma/events`) accepts the same shapes.
- Editing follows the same rules as any status: an edit **with** an
  `event` payload re-syncs the side tables (cancel/postpone/reschedule
  are ordinary Updates, never Deletes); an edit **without** the
  payload preserves the stored event metadata.
- Removing an invitee in an edit deletes their pending `invited` row
  and drops their mention. An invitee who already responded
  (accepted, tentative or declined) keeps that RSVP row, and loses
  visibility of the event, so the organizer's participant list can
  still show them.
- Moving the start time auto-fills `previous_start_time`, sets
  `status = rescheduled` (unless an explicit status was sent) and
  re-arms reminders.
- Title falls back to the status text on create and to the stored
  title on edit.
- Place locations carry structured address parts (§9.1 input
  spellings); the read entity emits them under
  `yonder.event.locations[].address` as a nested §9.2 object
  (`street`/`locality`/`region`/`postal_code`/`country`/`text`). The
  composer hydrates that shape back into the flat draft fields on
  edit.
- The composer offers address autocomplete on place locations when
  `[events.geocoding]` names a provider: `provider = "nominatim"`
  proxies `nominatim_url` (a descriptive `user_agent` is required by
  their usage policy — self-hosting is recommended), `provider =
  "google"` calls the Geocoding API with `google_api_key` (prefer the
  `YONDER_EVENTS__GEOCODING__GOOGLE_API_KEY` env var). Both are
  normalized server-side by `events::geocoding` and served to the SPA
  at `GET /api/v1/yonder/geocoding/search?q=` (authenticated,
  `search` rate-limit bucket); the instance entity advertises the
  provider as `yonder.events_geocoding` so the field only renders when
  lookup is configured. A picked suggestion fills every address part
  the hit knows, including coordinates.
- Invitees require a `direct`-visibility event and existing accounts —
  local or remote (a remote stub row exists once the actor is known,
  e.g. via `GET /api/v1/accounts/lookup` or search `resolve`). Invitees
  become mentions (visibility + Mastodon-compatible mention
  notification) **and** receive an ActivityPub `Invite` plus an
  `invited` participation row — remote invitees only see the `Invite`
  (their instance fetches the event object signed by its instance
  actor, which `serve_object` authorizes when the signer domain has a
  mentioned recipient). Invite responses naming the `Invite` activity
  id resolve through `invite_activity_uri`. Adding invitees in a later
  edit only notifies the newly added ones.

## RSVP

`POST /api/v1/yonder/events/{id}/rsvp` with
`{"state": "accepted" | "tentative" | "declined" | "none", "message"}`
drives the §7.1 state machine — the same one inbound ActivityPub
`Join`/`Leave`/`Accept`/`Reject`/`TentativeAccept` activities use.
Join modes: `free` (instant), `restricted` (organizer approval via
`POST .../participation_requests/{account_id}/{authorize|reject}`),
`invite` (invited rows only), `external` (redirect to
`externalParticipationUrl`). Organizer extras: `GET .../participations`,
`POST .../invitees` (direct-visibility events only — same rule as
authoring), `DELETE .../participants/{account_id}`.

Outbound responses follow the remote event's protocol: `Join`/`Leave`
(Mobilizon-style) or `Accept`/`Reject`/`TentativeAccept` on the
organizer's `Invite` (Friendica-style). Answering an `invited` row on a
remote event sends that response back to the organizer actors, using
the stored `invite_activity_uri` as the object when present.

## Notifications

Four `pleroma:*` notification types (all toggleable in the push
settings):

| Type | Recipient | When |
|---|---|---|
| `pleroma:participation_request` | organizer(s) | pending join on a restricted/invite event |
| `pleroma:participation_accepted` | participant | invite response, organizer authorize |
| `pleroma:event_update` | local participants | event edit, remote sync, deletion |
| `pleroma:event_reminder` | accepted/tentative participants | `reminder_lead_minutes` before start |

Event updates also emit the standard Mastodon `update` notification.
The reminder sweep (`event_reminder_sweep` job, cron-configurable)
is idempotent via `event_participations.reminded_at`; reschedules
clear the stamp. A participant viewing a stale remote event enqueues a
bounded re-fetch (`EventRemoteRefresh`).

## Calendar

- Per-event ICS: `/api/v1/yonder/events/{id}/ics` and the
  Nicolex-compatible `/api/v1/pleroma/events/{id}/ics`.
- Anonymous profile feed: `/users/{u}/events.ics` (public upcoming
  events).
- **Personalized subscription**: `GET /users/{username}/calendar/{token}.ics`
  — organized *and* participating events with the viewer's RSVP as
  `ATTENDEE;PARTSTAT` (`NEEDS-ACTION`/`ACCEPTED`/`TENTATIVE`/`DECLINED`)
  and `ORGANIZER` on owned events. Manage the token from the Events
  page (`POST`/`DELETE /api/v1/yonder/calendar/token`); the plaintext
  URL is shown once at mint/rotate time, only the SHA-256 hash is
  stored, and revocation is immediate. Works as an "add by URL"
  subscription in Google Calendar, Nextcloud, Outlook and Apple
  Calendar.
  - The token is the authorization, so the feed is **not** filtered by
    status visibility: it includes followers-only, direct and unlisted
    events the viewer organizes or was invited to or responded to.
    This is an exception to the discovery Public-only rule above.
  - The feed covers events that ended within the past year and is
    capped at 1000 events: upcoming events first (nearest start
    first), then past events (most recent first). An overflowing cap
    therefore drops the oldest past events, never the imminent ones.
    Postponed events (no start time) are omitted.

## Discovery

- `GET /api/v1/yonder/events` — chronological listing with scopes
  `mine`, `participating` (+ `state` filter), `home` (followed
  accounts), `local`, `federated`, `account:{id}`; filters `category`,
  `tag`, `online`, `near=lat,lon` + `radius_km` (bounding-box
  prefilter + great-circle distance), `from`/`to` window (explicit
  `from` without `to` is unbounded), `include_cancelled`; keyset
  pagination via `cursor`.
- Scope visibility: `federated` returns **public** events from
  anywhere; `local` returns **public and local-visibility** events
  from this instance. Neither includes `unlisted` (or anything
  narrower) — discovery output never carries events without Public
  in `to`. (The personal calendar subscription below is the one
  exception.)
- `/events` — the personal page (organizing / going / pending /
  invited tabs, calendar subscription block).
- `/events/discover` — the public discovery page (Upcoming / Local /
  Federated tabs; signed-in viewers land on Upcoming, anonymous
  visitors on Local; category/hashtag/online/near-me filters). Gated by
  `events.enabled` like every other event surface.
- `/@:acct/events` — profile Events tab. Served by the list API
  (`scope=account:{id}`), not the `upcomingEvents` collection, so it
  honours viewer visibility and is not capped like the collection.
  The default window is now → +90d, so the tab shows *upcoming*
  events only; past events require an explicit `from`, which the SPA
  does not send.
- Actor advertisement: the actor document carries `upcomingEvents`
  pointing at `GET /ap/users/{u}/collections/upcoming_events`
  (OrderedCollection, ascending by start time, public/unlisted only —
  same scoping as `featured`; followers-only events are not included
  because the collection is served through the shared document cache
  and cannot vary per signer).
  - **Cap:** at most 200 events (the nearest upcoming ones). There is
    no pagination; `totalItems` counts the items actually listed.
  - **Staleness:** the document is cached and invalidated on the
    author's event create, edit and delete. The end-time filter is
    applied at render time, so an event can stay listed after it has
    ended until the cache entry expires.
  - **Cancelled events are listed.** The collection excludes
    tombstones and postponed events, not cancellations; the object's
    `eventStatus` carries the cancellation, so remote clients can
    render it. Discovery, by contrast, hides cancelled events unless
    `include_cancelled=true`.
  - **Errors:** a genuine miss is a cacheable 404; any other failure
    is a non-cacheable 500.

## Rendering

The status card renders start and end on separate labelled lines as
`<date> <time> (<IANA zone>)` (e.g. `Europe/Amsterdam`, plus the
viewer's local time when the zones differ). Place locations link to
the configured `[events] map_provider` — `osm` (default,
openstreetmap.org, coordinates preferred with an address-text search
fallback) or `google` (Google Maps search URL, no API key needed) —
and a map button opens an embedded OpenStreetMap preview in a modal.
Every event also offers **Copy link** (the status permalink) and
**Download calendar** (the per-event ICS feed — a plain link for
public/unlisted events, an authenticated download otherwise) for
clients that can't render events natively; the same pair is emitted
as `Link` attachments on the ActivityPub object.

## Federation

Actor documents are not served with conditional-GET support: Yonder
adds `upcomingEvents` after pubby computes its validator, so the actor
document always returns `200` with a fresh ETag, never `304`.

Inbound: FEP-8a8e `Event` objects (Mobilizon, Gancio, WordPress Event
Bridge, Nicolex shapes) are parsed leniently and materialized into the
same side tables; `Invite`/`Join`/`Leave`/`Accept`/`Reject`/
`TentativeAccept` drive the RSVP machine; `Update` re-syncs and
notifies participants; `Delete` tombstones. Outbound: see
[FEDERATION.md](../FEDERATION.md).
