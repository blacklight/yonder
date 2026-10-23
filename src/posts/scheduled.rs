//! Scheduled statuses — create/list/get/update/cancel plus the
//! due-time-aware publish job and its recovery sweep (epic 12).
//!
//! The validated create payload lives in `scheduled_statuses.params`;
//! media ownership lives in `scheduled_status_media` (one row per
//! attachment — never duplicated into params). Publishing re-runs the
//! full create pipeline inside a transaction that moves the media
//! references onto the new status and deletes the scheduled row
//! (cascade clears the join).
//!
//! The delayed Redis entry can be lost or duplicated, so:
//! - the publish handler `SELECT … FOR UPDATE`s the row and refuses to
//!   publish early — a rescheduled-later row re-enqueues for its
//!   remaining delay and a re-run finding no row acks;
//! - a cron sweep (`content.scheduled_status_sweep_schedule`)
//!   re-enqueues every due row claimed `FOR UPDATE SKIP LOCKED`.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    jobs::{JobContext, JobHandler, JobKind},
    Error, Result,
};

use super::{create::ValidatedStatus, entity::ScheduledRow, mentions, render, PostContext};

/// Minimum lead for a post to count as "scheduled" — anything earlier
/// publishes immediately (Mastodon behavior).
pub fn is_future(at: DateTime<Utc>) -> bool {
    at > Utc::now()
}

/// Load a scheduled status by id.
pub async fn load(ctx: &PostContext, id: i64) -> Result<Option<ScheduledRow>> {
    let row = sqlx::query(
        "SELECT id, account_id, scheduled_at, params FROM scheduled_statuses WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&ctx.db)
    .await?;
    Ok(row.map(|r| ScheduledRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        scheduled_at: r.get("scheduled_at"),
        params: r.get("params"),
    }))
}

/// List the caller's scheduled statuses, soonest first.
pub async fn list(ctx: &PostContext, account_id: i64) -> Result<Vec<ScheduledRow>> {
    let rows = sqlx::query(
        "SELECT id, account_id, scheduled_at, params FROM scheduled_statuses \
         WHERE account_id = $1 ORDER BY scheduled_at LIMIT 100",
    )
    .bind(account_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ScheduledRow {
            id: r.get("id"),
            account_id: r.get("account_id"),
            scheduled_at: r.get("scheduled_at"),
            params: r.get("params"),
        })
        .collect())
}

/// Park a validated create for `scheduled_at`: insert the row, reserve
/// each media id through `scheduled_status_media`, enqueue the delayed
/// publish job.
pub async fn create_scheduled(
    ctx: &PostContext,
    account_id: i64,
    validated: &ValidatedStatus,
    scheduled_at: DateTime<Utc>,
    idem_key: Option<&str>,
) -> Result<ScheduledRow> {
    let params = serde_json::to_value(validated).map_err(|e| Error::Internal(e.to_string()))?;
    let mut tx = ctx.db.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO scheduled_statuses (account_id, scheduled_at, params) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(account_id)
    .bind(scheduled_at)
    .bind(&params)
    .fetch_one(&mut *tx)
    .await?;
    for media_id in validated.media_ids.as_deref().unwrap_or_default() {
        sqlx::query(
            "INSERT INTO scheduled_status_media (scheduled_status_id, media_attachment_id) \
             VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(key) = idem_key {
        sqlx::query(
            "UPDATE status_idempotency_keys SET target_type = 'scheduled_status', target_id = $3 \
             WHERE account_id = $1 AND idempotency_key = $2",
        )
        .bind(account_id)
        .bind(key)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let delay = (scheduled_at - Utc::now()).to_std().unwrap_or_default();
    ctx.dispatcher
        .enqueue_delayed(
            JobKind::ScheduledStatusPublish,
            json!({"scheduled_status_id": id}),
            delay,
        )
        .await?;
    Ok(ScheduledRow {
        id,
        account_id,
        scheduled_at,
        params,
    })
}

/// `PUT /api/v1/scheduled_statuses/{id}` — update `scheduled_at` and
/// re-enqueue for the new delay. A stale earlier job is neutralized by
/// the publish handler's due-time check.
pub async fn update(
    ctx: &PostContext,
    account_id: i64,
    id: i64,
    scheduled_at: DateTime<Utc>,
) -> Result<ScheduledRow> {
    if scheduled_at <= Utc::now() {
        return Err(Error::Validation(
            "scheduled_at must be in the future".to_string(),
        ));
    }
    let changed: Option<(i64, DateTime<Utc>, Value)> = sqlx::query_as(
        "UPDATE scheduled_statuses SET scheduled_at = $3, updated_at = now() \
         WHERE id = $1 AND account_id = $2 RETURNING id, scheduled_at, params",
    )
    .bind(id)
    .bind(account_id)
    .bind(scheduled_at)
    .fetch_optional(&ctx.db)
    .await?;
    let Some((id, at, params)) = changed else {
        return Err(Error::NotFound("scheduled status".into()));
    };
    let delay = (at - Utc::now()).to_std().unwrap_or_default();
    ctx.dispatcher
        .enqueue_delayed(
            JobKind::ScheduledStatusPublish,
            json!({"scheduled_status_id": id}),
            delay,
        )
        .await?;
    Ok(ScheduledRow {
        id,
        account_id,
        scheduled_at: at,
        params,
    })
}

/// `DELETE /api/v1/scheduled_statuses/{id}` — cancel; the media
/// reservations release via the join's cascade.
pub async fn cancel(ctx: &PostContext, account_id: i64, id: i64) -> Result<()> {
    let gone = sqlx::query("DELETE FROM scheduled_statuses WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(account_id)
        .execute(&ctx.db)
        .await?
        .rows_affected()
        > 0;
    if !gone {
        return Err(Error::NotFound("scheduled status".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publish job + sweep
// ---------------------------------------------------------------------------

/// `JobKind::ScheduledStatusPublish` — `{scheduled_status_id}`
/// publishes one row; an empty payload sweeps every due row (the cron
/// recovery path).
pub struct ScheduledStatusPublishJob {
    ap: Option<crate::activitypub::ApContext>,
}

impl ScheduledStatusPublishJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<crate::activitypub::ApContext>) -> Self {
        ScheduledStatusPublishJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ScheduledStatusPublishJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let post_ctx = PostContext::from_job(ctx, self.ap.clone());
        match payload.get("scheduled_status_id").and_then(Value::as_i64) {
            Some(id) => publish_one(&post_ctx, id).await,
            None => sweep(&post_ctx).await,
        }
    }
}

/// Rows a single sweep run re-enqueues.
const SWEEP_BATCH: i64 = 200;

/// Re-enqueue every due row. The publish handler is due-time aware and
/// idempotent, so the sweep may safely over-enqueue; `SKIP LOCKED`
/// prevents two workers from claiming the same row.
async fn sweep(ctx: &PostContext) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM scheduled_statuses WHERE scheduled_at <= now() \
         ORDER BY scheduled_at LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(SWEEP_BATCH)
    .fetch_all(&ctx.db)
    .await?;
    if ids.is_empty() {
        return Ok(());
    }
    tracing::info!(count = ids.len(), "sweeping due scheduled statuses");
    for id in ids {
        ctx.dispatcher
            .enqueue(
                JobKind::ScheduledStatusPublish,
                json!({"scheduled_status_id": id}),
            )
            .await?;
    }
    Ok(())
}

/// Publish one scheduled status: claim the row under a lock, refuse
/// to publish early after a reschedule (re-enqueue the remainder),
/// otherwise run the full create pipeline with the stored params +
/// reserved media in the same transaction that drops the row.
///
/// Mention resolution (which can block on remote WebFinger) and
/// rendering happen *before* the lock so no remote I/O runs inside
/// the transaction.
async fn publish_one(ctx: &PostContext, id: i64) -> Result<()> {
    // Read the params first — needed to resolve mentions outside the
    // lock; a missing row means already published/cancelled.
    let Some(pending) = load(ctx, id).await? else {
        return Ok(());
    };
    let mut validated: ValidatedStatus = serde_json::from_value(pending.params.clone())
        .map_err(|e| Error::Internal(e.to_string()))?;
    let media_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT media_attachment_id FROM scheduled_status_media WHERE scheduled_status_id = $1",
    )
    .bind(id)
    .fetch_all(&ctx.db)
    .await?;
    validated.media_ids = Some(media_ids);

    let account_id = pending.account_id;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&ctx.db)
        .await?;
    let parent = match validated.in_reply_to_id {
        Some(pid) => super::load(&ctx.db, pid).await?,
        None => None,
    };
    let resolved = mentions::resolve(ctx, &validated.status, account_id, parent.as_ref()).await?;
    let links: Vec<render::MentionLink> = resolved.iter().filter_map(|m| m.link()).collect();
    let mut rendered = render::render(
        &validated.status,
        &validated.content_type,
        &links,
        &ctx.base_url(),
        &ctx.config,
    );
    // Quote wiring — same as the immediate path: the Pleroma `RE:`
    // link, and a target that vanished since scheduling drops the
    // quote entirely.
    let quote_target = crate::interactions::quote::prepare_quote(
        ctx,
        &mut validated.quote_of_id,
        &mut rendered.html,
    )
    .await?;
    let deletion_days: i32 = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT scheduled_deletion_days FROM users WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(&ctx.db)
    .await?
    .flatten()
    .unwrap_or(0);

    // Claim + due-check under the lock, then persist.
    let mut tx = ctx.db.begin().await?;
    let due: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT scheduled_at FROM scheduled_statuses WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(scheduled_at) = due else {
        tx.commit().await?;
        return Ok(());
    };
    if scheduled_at > Utc::now() {
        // Rescheduled later while this job was in flight — re-enqueue
        // for the remaining delay and don't publish early.
        tx.commit().await?;
        let delay = (scheduled_at - Utc::now()).to_std().unwrap_or_default();
        ctx.dispatcher
            .enqueue_delayed(
                JobKind::ScheduledStatusPublish,
                json!({"scheduled_status_id": id}),
                delay,
            )
            .await?;
        return Ok(());
    }
    let status_id = super::create::insert_status_tx(
        ctx,
        &mut tx,
        &username,
        account_id,
        &validated,
        &resolved,
        &rendered,
        deletion_days,
        None,
    )
    .await?;
    // Move each reserved media reference onto the new status, then drop
    // the scheduled row (its join rows cascade).
    sqlx::query("DELETE FROM scheduled_statuses WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let status = super::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after scheduled publish".to_string()))?;
    super::preview::enqueue(
        ctx,
        &status,
        validated.media_ids.as_deref().unwrap_or_default(),
    )
    .await;
    if let Some(ap) = &ctx.ap {
        if let Err(e) = crate::activitypub::posts::publish_create(ap, ctx, &status, &resolved).await
        {
            tracing::error!(status_id, error = %e, "AP Create publish failed (scheduled)");
        }
    }
    if let Some(target) = &quote_target {
        crate::interactions::quote::finish_quote(ctx, ctx.ap.as_ref(), &status, target).await;
    }
    Ok(())
}

/// Register this module's job handler.
pub(crate) fn register(
    registry: &mut crate::jobs::Registry,
    ap: Option<&crate::activitypub::ApContext>,
) {
    registry.register(
        JobKind::ScheduledStatusPublish,
        ScheduledStatusPublishJob::new(ap.cloned()),
    );
}
