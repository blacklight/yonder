//! FEP-8a8e Event support — data model.
//!
//! Events are statuses: `statuses.object_type = 'Event'` with the name
//! in `statuses.title` and the description in `content`/`content_html`,
//! plus an `events` side table (migration `0057_events`) and child
//! tables for locations, participations and organizers.

pub mod entity;
pub mod ics;
pub mod inbound;
pub mod jobs;
pub mod model;
pub mod remote;
pub mod rsvp;
pub mod summary;
