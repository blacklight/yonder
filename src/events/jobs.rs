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

/// `JobKind::EventRemoteRefresh` — registered stub until Section 12
/// implements participant-view refresh. A no-op `Ok` so an accidental
/// enqueue completes harmlessly instead of dead-lettering.
pub struct EventRemoteRefreshJob;

#[async_trait::async_trait]
impl JobHandler for EventRemoteRefreshJob {
    async fn run(&self, _ctx: &JobContext, _payload: &Value) -> Result<()> {
        tracing::debug!("event_remote_refresh is a stub until Section 12");
        Ok(())
    }
}

/// Register the event jobs (called by [`crate::cli::build_registry`]).
pub fn register(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::EventsBackfill, EventsBackfillJob::new());
    registry.register(JobKind::EventRemoteRefresh, EventRemoteRefreshJob);
}
