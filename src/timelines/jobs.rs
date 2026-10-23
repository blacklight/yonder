//! `TimelineFanOut` — write-side index maintenance (epic 15).
//!
//! Two payload shapes share the kind:
//!
//! - `{ "status_id": N }` — fan one status out to its author's local
//!   followers (and the author). Enqueued by every status-creation
//!   path: local publish, local boost, remote `Create`, remote
//!   `Announce`.
//! - `{ "backfill_for": F, "of": T }` — backfill local follower `F`'s
//!   index with `T`'s recent statuses (new follows and accepted
//!   remote follow requests).
//!
//! Both are idempotent: `ZADD` on an existing member is a no-op, and
//! the read path re-validates anything stale.

use serde_json::Value;

use crate::{
    jobs::{JobContext, JobHandler, JobKind},
    posts,
    timelines::fanout,
    Result,
};

/// One status to fan out, or one follower to backfill.
pub struct TimelineFanOutJob;

#[async_trait::async_trait]
impl JobHandler for TimelineFanOutJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let config = &ctx.config.timelines;
        if let Some(status_id) = payload.get("status_id").and_then(Value::as_i64) {
            let Some(status) = posts::load_any(&ctx.db, status_id).await? else {
                return Ok(());
            };
            // Tombstoned statuses were deleted between enqueue and
            // run; `direct` statuses never index (home DMs resolve
            // through the mention predicate at read time).
            if status.tombstone || status.visibility == "direct" {
                return Ok(());
            }
            fanout::add(&ctx.db, &ctx.redis, config, status.id, status.account_id).await
        } else if let (Some(follower), Some(followee)) = (
            payload.get("backfill_for").and_then(Value::as_i64),
            payload.get("of").and_then(Value::as_i64),
        ) {
            fanout::backfill(&ctx.db, &ctx.redis, config, follower, followee).await
        } else {
            Ok(())
        }
    }
}

/// Wire the handlers into the job registry.
pub(crate) fn register(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::TimelineFanOut, TimelineFanOutJob);
}
