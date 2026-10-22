//! Posting job handlers — scheduled per-user deletion and orphan
//! media cleanup (epic 12).
//!
//! `StatusDeletion` carries `{status_id}` for one status or an empty
//! payload for the cron sweep (`content.scheduled_deletion_sweep_schedule`),
//! which enqueues due `statuses.scheduled_deletion_at` rows in batches.
//! Long-horizon deletions use a sweep, not decade-long delayed jobs —
//! epic 11's precedent.
//!
//! `CleanupOrphanMedia` deletes `media_attachments` rows referenced by
//! neither a status nor `scheduled_status_media` and older than
//! `media.orphan_retention_days` (`media.orphan_cleanup_schedule`).

use serde_json::{json, Value};

use crate::{
    activitypub::ApContext,
    jobs::{JobContext, JobHandler, JobKind},
    Result,
};

use super::PostContext;

/// Sweep batch size.
const SWEEP_BATCH: i64 = 200;

/// `JobKind::StatusDeletion` — `{status_id}` tombstones one status;
/// `{}` sweeps and enqueues every due row.
pub struct StatusDeletionJob {
    ap: Option<ApContext>,
}

impl StatusDeletionJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        StatusDeletionJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for StatusDeletionJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let post_ctx = PostContext::from_job(ctx, self.ap.clone());
        match payload.get("status_id").and_then(Value::as_i64) {
            Some(id) => {
                // Idempotent: a tombstoned/foreign row is a no-op.
                if let Some(status) = super::load_any(&post_ctx.db, id).await? {
                    if !status.tombstone {
                        super::create::tombstone(&post_ctx, &status).await?;
                    }
                }
                Ok(())
            }
            None => {
                let ids: Vec<i64> = sqlx::query_scalar(
                    "SELECT id FROM statuses \
                     WHERE scheduled_deletion_at <= now() AND NOT tombstone \
                     ORDER BY scheduled_deletion_at LIMIT $1 FOR UPDATE SKIP LOCKED",
                )
                .bind(SWEEP_BATCH)
                .fetch_all(&ctx.db)
                .await?;
                if ids.is_empty() {
                    return Ok(());
                }
                tracing::info!(count = ids.len(), "sweeping scheduled status deletions");
                let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
                for id in ids {
                    dispatcher
                        .enqueue(JobKind::StatusDeletion, json!({"status_id": id}))
                        .await?;
                }
                Ok(())
            }
        }
    }
}

/// `JobKind::CleanupOrphanMedia` — delete unattached uploads past the
/// retention window (rows referenced by neither a status nor a
/// scheduled status).
pub struct CleanupOrphanMediaJob;

#[async_trait::async_trait]
impl JobHandler for CleanupOrphanMediaJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let retention = ctx.config.media.orphan_retention_days;
        let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT id, file_path FROM media_attachments \
             WHERE status_id IS NULL \
               AND created_at < now() - $1::int * interval '1 day' \
               AND id NOT IN (SELECT media_attachment_id FROM scheduled_status_media) \
             LIMIT $2",
        )
        .bind(retention as i32)
        .bind(SWEEP_BATCH)
        .fetch_all(&ctx.db)
        .await?;
        for (id, file_path) in rows {
            if let Some(key) = &file_path {
                if let Err(e) = crate::media::storage::delete_key(&ctx.config, key).await {
                    tracing::warn!(key, error = %e, "orphan media object delete failed");
                }
            }
            sqlx::query("DELETE FROM media_attachments WHERE id = $1")
                .bind(id)
                .execute(&ctx.db)
                .await?;
        }
        Ok(())
    }
}

/// Register this epic's handlers (called by [`crate::posts::register_jobs`]).
pub(crate) fn register(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(JobKind::StatusDeletion, StatusDeletionJob::new(ap.cloned()));
    registry.register(JobKind::CleanupOrphanMedia, CleanupOrphanMediaJob);
    super::scheduled::register(registry, ap);
    super::preview::register(registry, ap);
}
