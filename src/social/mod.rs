//! Social graph (epic 14) — follows, follow requests, blocks, mutes,
//! domain blocks, lists, user notes, subscriptions and counters.
//!
//! The write primitives live in [`crate::users::relationships`]
//! (epic 11): this module adds the REST-facing services on top —
//! the `Relationship` entity renderer ([`entity`]), shared visibility
//! predicates ([`visibility`]), the `Block`/`Undo(Block)` federation
//! that pubby doesn't build, the inbound `Follow`/`Block` hook arms
//! ([`inbound`]) and the `FollowEvent` deferred job ([`jobs`]).
//!
//! Invariants:
//! - `followers_count`/`following_count` for **local** accounts are
//!   maintained by the `follows` triggers (migration `0016`), never by
//!   app code. Remote counts come from actor documents.
//! - Notification *events* are emitted here (`follow`,
//!   `follow_request`); the `notifications` rows and their API are
//!   epic 16's.

pub mod blocks;
pub mod counters;
pub mod entity;
pub mod follows;
pub mod inbound;
pub mod jobs;
pub mod lists;
pub mod notes;
pub mod requests;
pub mod subscribe;
pub mod visibility;

pub use crate::users::relationships::{FollowOpts, FollowOutcome};

/// Register epic-14 job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    registry.register(crate::jobs::JobKind::FollowEvent, jobs::FollowEventJob);
}
