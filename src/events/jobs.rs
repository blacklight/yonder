//! Event background jobs — `EventsBackfill` re-materializes remote
//! Event objects ingested before the `events` side table existed
//! (they sit in `statuses` as converted notes), and
//! `EventRemoteRefresh` is reserved for the participant-view refresh
//! (Section 12 — currently the stub).

use serde_json::{json, Value};

use crate::{
    jobs::{Dispatcher, JobContext, JobHandler, JobKind},
    Result,
};

/// `JobKind::EventsBackfill` — paged one-off migration. Payload:
/// `{"after_id": <status id>, "batch_size": <n>}`; the root call
/// (`after_id` absent/0) scans forward and re-enqueues itself per
/// page, like `SearchIndexBackfillJob`.
///
/// Candidate rows: remote-authored (`accounts.domain IS NOT NULL`),
/// `object_type = 'Event'`, not tombstoned, created within the last
/// 12 months, and missing their `events` side-table row — i.e. every
/// remote event materialized before Section 2 shipped, plus any
/// ingest whose `sync_event` failed. Each URI is re-fetched through
/// `JobKind::RemoteStatusFetch` with `refresh: true`, which re-runs
/// the full parse + `sync_event` path; the handler's own markers,
/// circuit breaker and retry budget keep the run bounded and
/// idempotent.
pub struct EventsBackfillJob;

/// Statuses scanned per page before the job re-enqueues itself.
const DEFAULT_BATCH: i64 = 100;
const MAX_BATCH: i64 = 500;

impl EventsBackfillJob {
    pub fn new() -> Self {
        EventsBackfillJob
    }
}

impl Default for EventsBackfillJob {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl JobHandler for EventsBackfillJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let after_id = payload.get("after_id").and_then(Value::as_i64).unwrap_or(0);
        let batch = payload
            .get("batch_size")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_BATCH)
            .clamp(1, MAX_BATCH);
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT s.id, s.uri FROM statuses s \
             JOIN accounts a ON a.id = s.account_id \
             WHERE s.id > $1 \
               AND s.object_type = 'Event' \
               AND a.domain IS NOT NULL \
               AND NOT s.tombstone \
               AND s.uri IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM events e WHERE e.status_id = s.id) \
               AND s.created_at > now() - interval '12 months' \
             ORDER BY s.id LIMIT $2",
        )
        .bind(after_id)
        .bind(batch)
        .fetch_all(&ctx.db)
        .await?;
        if rows.is_empty() {
            tracing::info!(after_id, "events backfill complete");
            return Ok(());
        }
        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        let mut enqueued = 0u64;
        for (_id, uri) in &rows {
            // `refresh: true` bypasses the already-materialized
            // shortcut — the whole point is re-running parse+sync.
            // Enqueue failures propagate so the page retries; a
            // per-URI dedupe marker is unnecessary because the fetch
            // job is itself idempotent.
            dispatcher
                .enqueue(
                    JobKind::RemoteStatusFetch,
                    json!({"uri": uri, "refresh": true}),
                )
                .await?;
            enqueued += 1;
        }
        let last = rows.last().map(|(id, _)| *id).unwrap_or(after_id);
        tracing::info!(enqueued, after_id = last, "events backfill page");
        if rows.len() as i64 == batch {
            dispatcher
                .enqueue(
                    JobKind::EventsBackfill,
                    json!({"after_id": last, "batch_size": batch}),
                )
                .await?;
        }
        Ok(())
    }
}

/// `JobKind::EventRemoteRefresh` — participant-view refresh (Section
/// 12). The entity layer enqueues `{status_id}` when a participant
/// views a remote event older than
/// `events.remote_refresh_after_minutes`; the handler re-validates
/// (remote, not tombstoned, has an `events` row, still stale, at least
/// one local participant) and delegates the actual fetch to
/// `RemoteStatusFetch` with `refresh: true`, which reuses the handler's
/// markers, circuit breaker and retry budget.
pub struct EventRemoteRefreshJob;

#[async_trait::async_trait]
impl JobHandler for EventRemoteRefreshJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let status_id = payload
            .get("status_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| crate::Error::Validation("status_id required".into()))?;
        let row: Option<(bool, bool, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT s.local, s.tombstone, e.updated_at FROM statuses s \
             JOIN events e ON e.status_id = s.id WHERE s.id = $1",
        )
        .bind(status_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some((local, tombstone, updated_at)) = row else {
            return Ok(());
        };
        if local || tombstone {
            return Ok(());
        }
        let stale = updated_at
            < chrono::Utc::now()
                - chrono::Duration::minutes(i64::from(
                    ctx.config.events.remote_refresh_after_minutes,
                ));
        if !stale {
            return Ok(());
        }
        // Only refresh when a local account actually participates —
        // nobody is watching an event with no local audience.
        let has_local_participant: bool = sqlx::query_scalar(
            "SELECT EXISTS( \
               SELECT 1 FROM event_participations p \
               JOIN accounts a ON a.id = p.account_id AND a.domain IS NULL \
               WHERE p.status_id = $1)",
        )
        .bind(status_id)
        .fetch_one(&ctx.db)
        .await?;
        if !has_local_participant {
            return Ok(());
        }
        let uri: Option<String> = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
            .bind(status_id)
            .fetch_optional(&ctx.db)
            .await?
            .flatten();
        let Some(uri) = uri else {
            return Ok(());
        };
        Dispatcher::new(&ctx.redis, &ctx.config.jobs)
            .enqueue(
                JobKind::RemoteStatusFetch,
                json!({"uri": uri, "refresh": true}),
            )
            .await?;
        Ok(())
    }
}

/// `JobKind::EventReminderSweep` — periodic sweep emitting
/// `pleroma:event_reminder` for participations whose event starts
/// inside `events.reminder_lead_minutes`. Payload is `{}`; the
/// `reminded_at` stamp makes re-runs idempotent and `rearm_reminders`
/// re-arms on reschedule.
pub struct EventReminderSweepJob;

/// Rows processed per run — the sweep re-enqueues itself with an
/// `after` cursor when it hits the cap so a large backlog drains
/// across ticks instead of one long job.
const REMINDER_BATCH: i64 = 500;

#[async_trait::async_trait]
impl JobHandler for EventReminderSweepJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let cursor = payload.get("cursor").cloned();
        let rows = reminder_page(&ctx.db, &ctx.config.events, cursor.as_ref()).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        let mut last_cursor: Option<Value> = None;
        let mut emitted = 0u64;
        let mut failed = 0u64;
        for (status_id, account_id, start_time) in &rows {
            // The cursor must always advance past a consumed row —
            // even a skipped one — or a page of missing statuses
            // would restart the sweep from the beginning forever.
            last_cursor = Some(json!({
                "start": start_time.to_rfc3339(),
                "status_id": status_id,
                "account_id": account_id,
            }));
            let Some(status) = crate::posts::load(&ctx.db, *status_id).await? else {
                continue;
            };
            // Per-row failures are logged and left unstamped
            // (`reminded_at` stays unset, so the next sweep retries
            // them) instead of aborting the whole page — a persistent
            // enqueue failure on one participation must not starve
            // every reminder after it.
            match crate::events::notify::emit_event_reminder(
                &ctx.db,
                &dispatcher,
                &status,
                *account_id,
            )
            .await
            {
                Ok(()) => {
                    match crate::events::notify::mark_reminded(&ctx.db, *status_id, *account_id)
                        .await
                    {
                        Ok(()) => emitted += 1,
                        // Sent but not stamped: the next sweep may repeat
                        // it. Logged rather than aborting the page.
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(
                                status_id,
                                account_id,
                                error = %e,
                                "event reminder sent but not stamped"
                            );
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        status_id,
                        account_id,
                        error = %e,
                        "event reminder emission failed"
                    );
                }
            }
        }
        tracing::debug!(emitted, failed, "event reminder sweep page");
        if rows.len() as i64 == REMINDER_BATCH {
            dispatcher
                .enqueue(JobKind::EventReminderSweep, json!({"cursor": last_cursor}))
                .await?;
        }
        Ok(())
    }
}

/// One keyset page of reminder candidates. The cursor is the last
/// row's `(start_time, status_id, account_id)` triple — events share
/// start times across participants, so all three are needed.
async fn reminder_page(
    db: &sqlx::PgPool,
    events_cfg: &crate::config::EventsConfig,
    cursor: Option<&Value>,
) -> Result<Vec<(i64, i64, chrono::DateTime<chrono::Utc>)>> {
    let (start, sid, aid) = match cursor {
        Some(c) => (
            c.get("start")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            c.get("status_id").and_then(Value::as_i64),
            c.get("account_id").and_then(Value::as_i64),
        ),
        None => (None, None, None),
    };
    let rows: Vec<(i64, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT p.status_id, p.account_id, e.start_time FROM event_participations p \
         JOIN events e ON e.status_id = p.status_id \
         JOIN statuses s ON s.id = p.status_id AND NOT s.tombstone \
         WHERE p.state IN ('accepted','tentative') \
           AND p.reminded_at IS NULL \
           AND e.status NOT IN ('cancelled','postponed') \
           AND e.start_time IS NOT NULL \
           AND e.start_time > now() \
           AND e.start_time <= now() + make_interval(mins => $1) \
           AND ($2::timestamptz IS NULL \
                OR (e.start_time, p.status_id, p.account_id) \
                   > ($2, $3, $4)) \
         ORDER BY e.start_time, p.status_id, p.account_id LIMIT $5",
    )
    .bind(i64::from(events_cfg.reminder_lead_minutes))
    .bind(start)
    .bind(sid)
    .bind(aid)
    .bind(REMINDER_BATCH)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Register the event jobs (called by [`crate::cli::build_registry`]).
pub fn register(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::EventsBackfill, EventsBackfillJob::new());
    registry.register(JobKind::EventRemoteRefresh, EventRemoteRefreshJob);
    registry.register(JobKind::EventReminderSweep, EventReminderSweepJob);
}
