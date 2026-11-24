# Federation

Yonder is an ActivityPub server (Mastodon-compatible API surface,
pubby-based actor/document serving). This file records what Yonder
federates, which FEPs it implements, and the event-specific
interoperability notes (FEP-8a8e).

## Implemented specifications

| Spec | Support |
|---|---|
| ActivityPub / ActivityStreams 2.0 | actors, objects, inbox/outbox, `Follow`/`Accept`/`Undo`, `Create`/`Update`/`Delete`/`Announce`, `Like`/`EmojiReaction` |
| WebFinger + HTTP Signatures (draft-cavage) | full, incl. Ed25519 key advertisement |
| Mastodon API | statuses, timelines, notifications, media, polls, follows, blocks/mutes, reports, markers, filters, push |
| FEP-0449 (quote controls) | quotes with consent |
| FEP-044f (QuoteRequest) | outbound quote requests |
| FEP-8a8e (FEP for Events) | implemented; live interop pending — see below |
| Webmention | incoming verification + outgoing notifications |
| NodeInfo 2.0/2.1 | yes |

## FEP-8a8e: Events

### Outbound

- Events are `Event` objects: `name`, generated `summary` (preamble +
  excerpt), `startTime`/`endTime` in the event's IANA `timezone`,
  `displayEndTime`, `eventStatus` + iCal `status` companions,
  `previousStartTime` on reschedules, `location` (array of `Place` /
  `VirtualLocation` with `sc:address` parts), `isOnline`, `category`
  (string or array), `joinMode`
  (`free|restricted|external|invite`), `externalParticipationUrl`,
  `maximumAttendeeCapacity`/`remainingAttendeeCapacity`,
  `requiredJoinVisibility` (organizer) for approval-gated events,
  banner as `image`, flyer as attachment, `Link` attachments for the
  permalink and the per-event ICS feed (`mediaType:
  text/calendar` — clients that can't render events still get copy /
  download affordances), and `attendees`/`organizers` collection
  stubs on the object.
- The actor document advertises `upcomingEvents`
  (`/ap/users/{u}/collections/upcoming_events` — OrderedCollection,
  ascending by `startTime`, public/unlisted only, postponed excluded).
- RSVP activities: `Join{object: event, participationMessage?}`,
  `Leave`, `Accept`/`Reject`/`Ignore`/`TentativeAccept` (both
  Mobilizon-style join answers and Friendica-style invite answers),
  and `Invite{object: event, to: [invitee]}` for invitees-only
  events. All carry the FEP-8a8e `@context`.
- Audience: participants are added to the `cc` of event `Update`s;
  `Invite`s are addressed to the invitee only. DM invites are `direct`
  Notes mentioning the invitees, carrying `yonder:inviteTo` and
  `catodon:inviteTo` with the event URI (Catodon interop).

### Inbound

- `Event` objects from Mobilizon, Gancio, WordPress Event Bridge and
  Nicolex/pleroma-style payloads are parsed leniently (string-or-array
  `category`, `ical:status` fallback, all-null `PostalAddress`
  handling, group `attributedTo` ownership) and materialized into
  structured side tables; unknown category tokens are preserved.
- RSVP activities drive the same state machine as the local API:
  `Join`/`Leave`/`Accept`/`Reject`/`TentativeAccept`/`Invite` on local
  events, with `Accept`/`Reject` responses published back for remote
  joins where the join mode calls for it.
- Two invite mechanisms inbound: Friendica/Hubzilla `Invite{Event}`
  (organizer-authorized, `invited` rows) and Catodon `inviteTo` DMs —
  `Create` notes carrying `yonder:inviteTo`/`catodon:inviteTo` write
  `invited` rows for their local mentions, parking in
  `event_pending_dm_invites` until the event federates.
- `Update` on a known event re-syncs the side tables, notifies local
  participants (`update` + `pleroma:event_update`) and re-arms
  reminders only when `startTime` actually moved. `Delete` tombstones
  and notifies before the scrub.
- `attachment[]` `Link` entries promote to virtual locations only when
  the event is online or has no physical location; calendar feeds
  (`text/calendar`, `.ics`) never promote, and a link matching the
  object's own `url`/`id` is dropped (it duplicates the permalink).
- An event's `summary` never becomes a content warning: it is the
  machine-generated when/where/description preamble (Yonder and Gancio
  emit it for Mastodon-style clients), which the event card already
  renders structured — the text is kept in `statuses.summary` for
  search only. Notes keep the usual summary-as-CW behaviour.

### JSON-LD context

Event documents and RSVP activities use pubby's AP context with the
FEP-8a8e / schema.org terms merged into its object member
(`fep8a8e:`, `sc:` → `http://schema.org/`, `mz:`, `ical:`). Activity
Streams terms (`location`, `Place`, `latitude`, `longitude`,
`startTime`, `endTime`, `name`) are never redefined, and
`https://schema.org` is never included as a context URL.

### Known interop caveats

- `previousStartTime` is emitted as `xsd:dateTime`; the FEP's example
  JSON-LD context types it as `@vocab` — Yonder follows the FEP text
  and the upstream context is expected to be corrected.
- Calendar `ATTENDEE`/`ORGANIZER` values use `https://` actor URIs
  rather than `mailto:` cal-addresses; RFC 5545 clients generally
  accept URIs, but import behaviour varies (see `docs/events.md`).
- `upcomingEvents` lists at most 200 nearest upcoming events with no
  pagination, and is cached: an event can remain listed until the
  cache entry expires after it ends. Actor documents never return
  `304` (the injected property invalidates pubby's validator).
- Live interop checks against Mastodon, Mobilizon, Gancio, Friendica,
  Akkoma, Nicolex/pl-fe and WordPress Event Bridge are still pending —
  the checklist lives in the project notes (`32-events/interop.md`)
  and is to be filled in on a deployed instance.
