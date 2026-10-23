//! Timelines (epic 15) — home, local, federated, list and tag reads.
//!
//! The read path is one SQL engine ([`filter`] + [`query`]) shared by
//! every scope; the home feed additionally keeps a Redis sorted-set
//! index per user, filled by `TimelineFanOut` jobs on publish
//! ([`fanout`], [`jobs`]). Postgres is always the source of truth —
//! the ZSET is a first-page accelerator whose contents are
//! re-validated by the same predicate, so a Redis outage degrades to
//! plain SQL, never a 5xx.
//!
//! Invariants:
//!
//! - **Visibility lives in [`filter`].** Public scopes serve `public`
//!   only (never `unlisted`, matching Mastodon's public streams);
//!   home/list admit follower-gated visibilities; `direct` reaches
//!   home only via a `status_mentions` row.
//! - **Moderation is read-time.** Blocks, mutes, per-user domain
//!   blocks and instance `domain_blocks` re-evaluate on every read —
//!   unfollows/mutes take effect without re-fanning out.
//! - **Filters are read-time too.** `filter_languages`, hide-boosts
//!   (`follows.show_reblogs`), exclusive lists and `only_media` are
//!   applied by [`filter`], not baked into the index.
//! - **Streaming is publish-site-only.** `update`/`delete` events go
//!   on the `EventBus` where the write happens ([`stream`]); job-side
//!   re-delivery is unnecessary because the bus already saw the write.

pub mod fanout;
pub mod filter;
pub mod jobs;
pub mod query;
pub mod scope;
pub mod stream;

pub use filter::Filters;
pub use scope::TimelineScope;

/// Register epic-15 job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    jobs::register(registry);
}
