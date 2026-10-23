//! `FollowEvent` — the deferred settle job for inbound `Follow`s.
//!
//! The `on_activity` hook can't tell whether pubby will land a
//! `follows` row (unlocked target → auto-accept) or a `follow_requests`
//! row (locked target), so it defers this job a couple of seconds and
//! lets the stored row decide which notification event to emit.
//! Idempotent by construction: re-runs re-derive the outcome from the
//! same tables.

use serde_json::Value;

use crate::{
    interactions::emit_notification_event,
    jobs::{Dispatcher, JobContext, JobHandler},
    Result,
};

/// Settle one inbound `Follow` — payload `{actor_id, target_actor_id}`
/// (AP actor URIs).
pub struct FollowEventJob;

#[async_trait::async_trait]
impl JobHandler for FollowEventJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(actor_id) = payload.get("actor_id").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let Some(target_actor_id) = payload.get("target_actor_id").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        // Resolve to local account ids — the target must be a live
        // local account; the requester row was upserted by pubby's
        // dispatch (or is absent for an already-deleted actor).
        let pair: Option<(i64, i64)> = sqlx::query_as(
            "SELECT a.id, t.id FROM accounts a, accounts t \
             WHERE a.actor_id = $1 AND t.actor_id = $2 AND t.domain IS NULL \
               AND t.suspended_at IS NULL AND t.deleted_at IS NULL",
        )
        .bind(actor_id)
        .bind(target_actor_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some((from_id, to_id)) = pair else {
            return Ok(());
        };

        // Which row did pubby land? `follows` → accepted; `follow_requests`
        // → pending approval. Neither → withdrawn already, no event.
        let kind: Option<String> = sqlx::query_scalar(
            "SELECT CASE \
                WHEN EXISTS(SELECT 1 FROM follows \
                            WHERE account_id = $1 AND target_account_id = $2) THEN 'follow' \
                WHEN EXISTS(SELECT 1 FROM follow_requests \
                            WHERE account_id = $1 AND target_account_id = $2) THEN 'follow_request' \
             END",
        )
        .bind(from_id)
        .bind(to_id)
        .fetch_one(&ctx.db)
        .await?;
        let Some(kind) = kind else {
            return Ok(());
        };

        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        emit_notification_event(&ctx.db, &dispatcher, &kind, from_id, None, to_id).await;
        Ok(())
    }
}
