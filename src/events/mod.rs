//! FEP-8a8e Event support — data model.
//!
//! Events are statuses: `statuses.object_type = 'Event'` with the name
//! in `statuses.title` and the description in `content`/`content_html`,
//! plus an `events` side table (migration `0057_events`) and child
//! tables for locations, participations and organizers.

pub mod api;
pub mod calendar;
pub mod dm_invites;
pub mod entity;
pub mod geocoding;
pub mod ics;
pub mod inbound;
pub mod jobs;
pub mod model;
pub mod notify;
pub mod remote;
pub mod rsvp;
pub mod summary;
pub mod validate;
