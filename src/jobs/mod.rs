//! Background-job queue, task dispatcher and scheduler.
//!
//! Jobs persist in Redis Streams so they survive process restarts;
//! duplicate delivery is tolerated, so **handlers must be idempotent**.
//!
//! - [`JobKind`] — the typed registry of job kinds, each routed to a
//!   [`Queue`] (`default` / `publish` / `federation` / `bulk` /
//!   `cleanup` / `timeline` / `notifications` / `remote_status` /
//!   `remote` / `remote_bulk`).
//! - [`JobHandler`] — implement it and [`Registry::register`] it to plug
//!   a real implementation in (later epics); [`stub`] registers
//!   placeholder handlers so the machinery works end-to-end today.
//! - [`Dispatcher`] — enqueue jobs for immediate or delayed execution.
//! - [`worker::Worker`] — claims, executes, acks/nacks with bounded
//!   retries, exponential backoff and dead-lettering.
//! - [`scheduler`] — cron schedules (from config) that enqueue the
//!   cleanup job kinds.

pub mod queue;
pub mod remote_gate;
pub mod scheduler;
pub mod stub;
pub mod worker;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::{cache::Redis, config::Config, Result};

/// Queue names — one Redis stream each.
///
/// - `default`: interactive / low-volume work that must not wait behind
///   remote fetches.
/// - `publish`: status publishing work — scheduled publishes and quote
///   authorization updates are user-visible and get their own lane.
/// - `federation`: latency-sensitive delivery — a remote inbox answer
///   minutes late is worthless, so it never waits behind bulk work.
/// - `bulk`: high-volume per-item jobs (imports, exports, link
///   previews, thread expansion) that would otherwise starve
///   everything else.
/// - `cleanup`: scheduled sweeps.
/// - `timeline`: home-timeline index writes.
/// - `notifications`: notification fan-out.
/// - `remote_status`: user-visible remote object fetches (quote/pin
///   targets, media/poll refreshes) — isolated from bulk remote work.
/// - `remote`: remote fetches and syncs that are allowed to lag
///   (actors, networks, media, incoming Webmentions).
/// - `remote_bulk`: large remote walks that can lag furthest behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Queue {
    /// Interactive, low-volume jobs.
    Default,
    /// Status publishing jobs.
    Publish,
    /// Latency-sensitive federation delivery.
    Federation,
    /// High-volume per-item jobs.
    Bulk,
    /// Scheduled cleanup sweeps — kept off `bulk` so a long-running
    /// cleanup can't starve per-item fan-out work.
    Cleanup,
    /// Home-timeline index writes — a post that lands minutes late in
    /// every follower's feed is stale, so it never waits behind
    /// remote fetches on `default`.
    Timeline,
    /// Notification fan-out — a notification that lands minutes late
    /// is stale, so it never waits behind remote fetches on `default`.
    Notifications,
    /// User-visible remote status fetches.
    RemoteStatus,
    /// Remote actor/network/media/Webmention work.
    Remote,
    /// Large remote ancestor walks.
    RemoteBulk,
}

impl Queue {
    /// Every queue the worker drains.
    pub const ALL: &'static [Queue] = &[
        Queue::Default,
        Queue::Publish,
        Queue::Federation,
        Queue::Bulk,
        Queue::Cleanup,
        Queue::Timeline,
        Queue::Notifications,
        Queue::RemoteStatus,
        Queue::Remote,
        Queue::RemoteBulk,
    ];

    /// Stream suffix.
    pub fn as_str(&self) -> &'static str {
        match self {
            Queue::Default => "default",
            Queue::Publish => "publish",
            Queue::Federation => "federation",
            Queue::Bulk => "bulk",
            Queue::Cleanup => "cleanup",
            Queue::Timeline => "timeline",
            Queue::Notifications => "notifications",
            Queue::RemoteStatus => "remote_status",
            Queue::Remote => "remote",
            Queue::RemoteBulk => "remote_bulk",
        }
    }

    /// Consumer tasks this queue gets. Per-queue overrides live in
    /// `jobs.*_concurrency`; unset lanes fall back to `jobs.concurrency`,
    /// with `remote` defaulting to double so slow remote fetches drain
    /// faster without stealing slots from latency-sensitive lanes.
    pub fn consumer_count(&self, jobs: &crate::config::JobsConfig) -> u32 {
        let base = jobs.concurrency.max(1);
        match self {
            Queue::Publish => jobs.publish_concurrency.unwrap_or(base),
            Queue::RemoteStatus => jobs.remote_status_concurrency.unwrap_or(base),
            Queue::Remote => jobs
                .remote_concurrency
                .unwrap_or_else(|| base.saturating_mul(2)),
            Queue::RemoteBulk => jobs.remote_bulk_concurrency.unwrap_or(base),
            Queue::Bulk => jobs.bulk_concurrency.unwrap_or(base),
            _ => base,
        }
    }

    /// Whether this queue carries remote-fetch work covered by the
    /// remote in-flight cap and enqueue dedupe.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            Queue::RemoteStatus | Queue::Remote | Queue::RemoteBulk
        )
    }
}

/// Every job kind the dispatcher accepts.
///
/// Handlers are registered per kind; this epic ships stubs for all of
/// them — later epics replace the stub with a real [`JobHandler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Deliver an ActivityPub activity to a remote inbox.
    FederationDelivery,
    /// Verify and process a queued inbound ActivityPub activity.
    FederationIncoming,
    /// Re-enqueue delivery-ledger rows whose queue entry was lost.
    FederationDeliverySweep,
    /// Fetch and cache a remote media attachment.
    RemoteMediaFetch,
    /// Fetch and cache a remote actor document (`{"actor_url": …}`),
    /// or sweep stale remote actors (`{}` — the
    /// `remote_actor_refresh_schedule` fan-out).
    RemoteActorFetch,
    /// WebFinger-resolve `@user@domain` and stub the actor
    /// (`{"username": …, "domain": …}`) — enqueued by interactive
    /// lookups whose inline WebFinger attempt missed or timed out, so
    /// resolution completes off the request path.
    RemoteActorResolve,
    /// Fetch and materialize a remote status object (`{"uri": …}`) —
    /// resolves pending quote targets whose servers never federate
    /// with us.
    RemoteStatusFetch,
    /// Refresh one remote account's cached followers/following edges
    /// (`{"account_id": …, "direction": "followers"|"following"}`) —
    /// enqueued by `accounts/{id}/followers|following` reads instead of
    /// syncing inline.
    RemoteNetworkSync,
    /// Fan a notification out to its recipients.
    NotificationFanout,
    /// Deliver one notification to the recipient's Web Push
    /// subscriptions (`{"notification_id": …, "account_id": …}`).
    WebPushSend,
    /// Expand one remote status's thread — ancestors, `replies`
    /// subtree to the configured depth and engagement counters —
    /// under the full walk budget (`{"status_id": …,
    /// "round": n}`). Enqueued as the continuation of a
    /// `GET /context` expansion that the quick budget truncated.
    ThreadExpand,
    /// Materialize the ancestor path of a remote reply whose parent
    /// wasn't local at ingest (`{"status_id": …}`) — ancestors only,
    /// the replies subtree stays lazy. On success the reply is
    /// relinked and fanned out.
    ThreadPath,
    /// Send an email.
    EmailSend,
    /// Import an account archive.
    Import,
    /// Export an account archive.
    Export,
    /// Prune remote activities no local user has interacted with.
    CleanupRemoteActivities,
    /// Evict stale cached remote media.
    CleanupRemoteMedia,
    /// Re-verify one account's link-valued profile fields
    /// (`rel="me"` — `{"account_id": …}`, local or remote), or sweep
    /// every local account's (epic 09).
    VerifyProfileLinks,
    /// Purge one deleted account, or sweep due `account_deletions`.
    AccountDeletion,
    /// Migrate local followers after a verified account move.
    MigrateFollowers,
    /// Delete expired archive-export files.
    ExportCleanup,
    /// Publish one scheduled status, or sweep due ones (epic 12).
    ScheduledStatusPublish,
    /// Fetch Open Graph link-preview metadata for a status (epic 12).
    FetchLinkPreview,
    /// Tombstone one status due for scheduled deletion, or sweep due
    /// ones (epic 12, `users.scheduled_deletion_days`).
    StatusDeletion,
    /// Delete unattached media uploads past the retention window
    /// (epic 12).
    CleanupOrphanMedia,
    /// Precompute the public instance counters served by
    /// `/api/v1/instance` (`{}` — the
    /// `instance.stats_refresh_schedule` fan-out).
    InstanceStatsRefresh,
    /// Settle an inbound `Follow`'s notification once pubby's dispatch
    /// has stored the row — emits `follow` or `follow_request`
    /// depending on which row landed (epic 14).
    FollowEvent,
    /// Fan a status out to follower home feeds, or backfill one
    /// follower's feed from a new followee (epic 15).
    TimelineFanOut,
    /// Drive one migration event's durable publication + migration
    /// enqueue (`{"event_id": …}`) — the retryable backstop for a move
    /// whose publish or `MigrateFollowers` enqueue failed.
    PublishMove,
    /// Delete one stored object once nothing references it — enqueued
    /// delayed for replaced/cleared profile images so the old URL
    /// survives the actor-`Update` propagation window (epic 18).
    DeleteMediaObject,
    /// Prune `audit_log` entries past `admin.audit_log_retention_days`
    /// (epic 21, `admin.audit_log_sweep_schedule`).
    AuditLogCleanup,
    /// Verify an inbound Webmention (`{"source": …, "target": …}`) —
    /// fetch the source page, confirm it links back, materialize the
    /// mention as a pseudo-actor reply.
    WebmentionIncoming,
    /// Deliver a local status's links to their Webmention endpoints
    /// (`{"status_id": …}`); also retracts mentions whose links were
    /// edited out or deleted.
    WebmentionOutgoing,
    /// Drain `publication_work` rows for one import session
    /// (`{"import_id": …}`) — the durable, once-only federation path
    /// for imports created with `federate` (epic 31 §8).
    ImportPublish,
    /// Re-enqueue import sessions whose job envelope was lost after
    /// the state write (`{}` — the `admin.import_sweep_schedule`
    /// fan-out).
    ImportSweep,
    /// Stamp a local quote's FEP-044f `QuoteAuthorization` and publish
    /// the status `Update` (`{"status_id": …, "authorization_uri": …}`).
    QuoteAuthorization,
    /// Keep the Elasticsearch/OpenSearch search index in sync
    /// (`{"entity": "status"|"account", "id": …, "op": "index"|"remove"}`)
    /// — enqueued by the write paths when `search.elasticsearch_url`
    /// is set; a no-op under the Postgres backend.
    SearchIndexSync,
    /// Bulk (re)index statuses/accounts into the ES backend
    /// (`{"entity": "statuses"|"accounts"|"all", "after_id": …,
    /// "batch_size": …}`) — cursor-paginates and re-enqueues itself.
    SearchIndexBackfill,
    /// Re-materialize remote Event statuses that predate the `events`
    /// side table (`{"after_id": …, "batch_size": …}` — paged,
    /// self-continuing). One-off backfill, also runnable via
    /// `yonder run-job events_backfill`.
    EventsBackfill,
    /// Re-fetch a remote event not refreshed within
    /// `events.remote_refresh_after_minutes` — enqueued by the entity
    /// layer when a participant views a stale remote event.
    EventRemoteRefresh,
    /// Periodic sweep emitting `pleroma:event_reminder` notifications
    /// for events starting inside `events.reminder_lead_minutes`
    /// (payload `{}` or `{"cursor": …}` for multi-page drains).
    EventReminderSweep,
}

impl JobKind {
    /// Every known job kind.
    pub const ALL: &'static [JobKind] = &[
        JobKind::FederationDelivery,
        JobKind::FederationIncoming,
        JobKind::FederationDeliverySweep,
        JobKind::RemoteMediaFetch,
        JobKind::RemoteActorFetch,
        JobKind::RemoteActorResolve,
        JobKind::RemoteStatusFetch,
        JobKind::RemoteNetworkSync,
        JobKind::NotificationFanout,
        JobKind::WebPushSend,
        JobKind::EmailSend,
        JobKind::Import,
        JobKind::Export,
        JobKind::CleanupRemoteActivities,
        JobKind::CleanupRemoteMedia,
        JobKind::VerifyProfileLinks,
        JobKind::AccountDeletion,
        JobKind::MigrateFollowers,
        JobKind::ExportCleanup,
        JobKind::ScheduledStatusPublish,
        JobKind::FetchLinkPreview,
        JobKind::StatusDeletion,
        JobKind::CleanupOrphanMedia,
        JobKind::InstanceStatsRefresh,
        JobKind::FollowEvent,
        JobKind::TimelineFanOut,
        JobKind::DeleteMediaObject,
        JobKind::AuditLogCleanup,
        JobKind::WebmentionIncoming,
        JobKind::WebmentionOutgoing,
        JobKind::ThreadExpand,
        JobKind::ThreadPath,
        JobKind::PublishMove,
        JobKind::ImportPublish,
        JobKind::ImportSweep,
        JobKind::QuoteAuthorization,
        JobKind::SearchIndexSync,
        JobKind::SearchIndexBackfill,
        JobKind::EventsBackfill,
        JobKind::EventRemoteRefresh,
        JobKind::EventReminderSweep,
    ];

    /// Stable wire name (`snake_case`).
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::FederationDelivery => "federation_delivery",
            JobKind::FederationIncoming => "federation_incoming",
            JobKind::FederationDeliverySweep => "federation_delivery_sweep",
            JobKind::RemoteMediaFetch => "remote_media_fetch",
            JobKind::RemoteActorFetch => "remote_actor_fetch",
            JobKind::RemoteActorResolve => "remote_actor_resolve",
            JobKind::RemoteStatusFetch => "remote_status_fetch",
            JobKind::RemoteNetworkSync => "remote_network_sync",
            JobKind::NotificationFanout => "notification_fanout",
            JobKind::WebPushSend => "web_push_send",
            JobKind::EmailSend => "email_send",
            JobKind::Import => "import",
            JobKind::Export => "export",
            JobKind::CleanupRemoteActivities => "cleanup_remote_activities",
            JobKind::CleanupRemoteMedia => "cleanup_remote_media",
            JobKind::VerifyProfileLinks => "verify_profile_links",
            JobKind::AccountDeletion => "account_deletion",
            JobKind::MigrateFollowers => "migrate_followers",
            JobKind::ExportCleanup => "export_cleanup",
            JobKind::ScheduledStatusPublish => "scheduled_status_publish",
            JobKind::FetchLinkPreview => "fetch_link_preview",
            JobKind::StatusDeletion => "status_deletion",
            JobKind::CleanupOrphanMedia => "cleanup_orphan_media",
            JobKind::InstanceStatsRefresh => "instance_stats_refresh",
            JobKind::FollowEvent => "follow_event",
            JobKind::TimelineFanOut => "timeline_fan_out",
            JobKind::DeleteMediaObject => "delete_media_object",
            JobKind::AuditLogCleanup => "audit_log_cleanup",
            JobKind::WebmentionIncoming => "webmention_incoming",
            JobKind::WebmentionOutgoing => "webmention_outgoing",
            JobKind::ThreadExpand => "thread_expand",
            JobKind::ThreadPath => "thread_path",
            JobKind::PublishMove => "publish_move",
            JobKind::ImportPublish => "import_publish",
            JobKind::ImportSweep => "import_sweep",
            JobKind::QuoteAuthorization => "quote_authorization",
            JobKind::SearchIndexSync => "search_index_sync",
            JobKind::SearchIndexBackfill => "search_index_backfill",
            JobKind::EventsBackfill => "events_backfill",
            JobKind::EventRemoteRefresh => "event_remote_refresh",
            JobKind::EventReminderSweep => "event_reminder_sweep",
        }
    }

    /// The queue this kind routes to by default.
    pub fn queue(&self) -> Queue {
        match self {
            JobKind::FederationDelivery
            | JobKind::FederationIncoming
            | JobKind::FederationDeliverySweep
            | JobKind::PublishMove => Queue::Federation,
            JobKind::ScheduledStatusPublish | JobKind::QuoteAuthorization => Queue::Publish,
            JobKind::RemoteStatusFetch => Queue::RemoteStatus,
            JobKind::RemoteMediaFetch
            | JobKind::RemoteActorFetch
            | JobKind::RemoteActorResolve
            | JobKind::RemoteNetworkSync
            | JobKind::EventRemoteRefresh
            | JobKind::WebmentionIncoming
            | JobKind::WebmentionOutgoing => Queue::Remote,
            JobKind::ThreadPath => Queue::RemoteBulk,
            JobKind::Import
            | JobKind::Export
            | JobKind::VerifyProfileLinks
            | JobKind::MigrateFollowers
            | JobKind::StatusDeletion
            | JobKind::ImportPublish
            | JobKind::FetchLinkPreview
            | JobKind::ThreadExpand
            | JobKind::SearchIndexSync
            | JobKind::SearchIndexBackfill
            | JobKind::EventsBackfill
            | JobKind::EventReminderSweep => Queue::Bulk,
            JobKind::CleanupRemoteActivities
            | JobKind::CleanupRemoteMedia
            | JobKind::InstanceStatsRefresh
            | JobKind::AccountDeletion
            | JobKind::ExportCleanup
            | JobKind::CleanupOrphanMedia
            | JobKind::AuditLogCleanup
            | JobKind::ImportSweep
            | JobKind::DeleteMediaObject => Queue::Cleanup,
            JobKind::TimelineFanOut => Queue::Timeline,
            JobKind::NotificationFanout => Queue::Notifications,
            _ => Queue::Default,
        }
    }
}

/// The serializable job — one stream entry carries one envelope as its
/// `job` field (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEnvelope {
    /// Unique job id (uuid v4).
    pub id: String,
    /// Job type tag.
    pub kind: JobKind,
    /// Queue the job is routed to.
    pub queue: Queue,
    /// Handler payload.
    pub payload: Value,
    /// Delivery attempts so far.
    pub attempts: u32,
    /// Attempts allowed before dead-lettering.
    pub max_attempts: u32,
    /// Enqueue time (unix seconds).
    pub enqueued_at: u64,
    /// Error from the last failed attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Redis dedupe key claimed at enqueue time for remote queues —
    /// released when the job acks or dead-letters, retained across
    /// retries so a queued/retrying job can't be duplicated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

impl JobEnvelope {
    fn new(kind: JobKind, queue: Queue, payload: Value, max_attempts: u32) -> Self {
        JobEnvelope {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            queue,
            payload,
            attempts: 0,
            max_attempts,
            enqueued_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            last_error: None,
            dedupe_key: None,
        }
    }
}

/// Services a handler may need — shared with every job execution.
#[derive(Clone)]
pub struct JobContext {
    /// PostgreSQL pool.
    pub db: PgPool,
    /// Redis handle (pool + key namespace).
    pub redis: Redis,
    /// Effective configuration.
    pub config: Arc<Config>,
    /// Media storage backend — media fetch/cleanup jobs go through it.
    pub media: crate::media::storage::MediaStore,
}

/// Handler for one [`JobKind`]. Implementations must be idempotent —
/// a job can be delivered more than once.
#[async_trait::async_trait]
pub trait JobHandler: Send + Sync {
    /// Execute the job. `Ok(())` acks it; `Err` retries or dead-letters
    /// it according to the envelope's attempt budget.
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()>;
}

/// `JobKind` → handler map. Register real handlers at startup; the
/// stub set ([`stub::register_all`]) keeps unimplemented kinds
/// deliverable.
#[derive(Default)]
pub struct Registry {
    handlers: HashMap<JobKind, Arc<dyn JobHandler>>,
}

impl Registry {
    /// Register `handler` for `kind`, replacing any previous handler.
    pub fn register<H>(&mut self, kind: JobKind, handler: H)
    where
        H: JobHandler + 'static,
    {
        self.handlers.insert(kind, Arc::new(handler));
    }

    /// The handler for `kind`, if any.
    pub fn handler(&self, kind: JobKind) -> Option<Arc<dyn JobHandler>> {
        self.handlers.get(&kind).cloned()
    }
}

/// How long a remote-job dedupe claim lives. It must outlast the
/// worst-case queue delay plus the retry budget; expiry only matters
/// if the job vanished without ack/dead-letter (a lost stream), where
/// a duplicate is safer than a permanently suppressed fetch.
///
/// Trade-off: if the process dies between a successful ack and the
/// best-effort `release_dedupe`, the claim survives until this TTL
/// and suppresses re-enqueues of the same `(kind, payload)` — for
/// `RemoteStatusFetch{uri}` that means a legitimate re-fetch waits up
/// to a day. The window is small (one Redis round-trip) and the
/// alternative (no dedupe) reintroduces the retry storms this fixes.
const REMOTE_JOB_DEDUPE_TTL_SECONDS: u64 = 24 * 3600;

/// Canonical JSON for dedupe hashing — `serde_json`'s
/// `preserve_order` feature keeps insertion order, so sort object keys
/// recursively before hashing.
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::new();
            for (key, child) in entries {
                out.insert(key.clone(), canonical_json(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

/// Stable dedupe key for a remote job: kind + canonical payload. Two
/// enqueues of the same work collide; different payloads (e.g. a
/// `RemoteStatusFetch` resolve vs `refresh`, or a `ThreadExpand` round)
/// stay distinct.
fn remote_dedupe_key(kind: JobKind, payload: &Value) -> String {
    let canonical = canonical_json(payload);
    let body = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hex::encode(hasher.finalize())[..32].to_string()
}

/// Outcome of claiming a remote-job dedupe slot.
enum DedupeClaim {
    /// This caller owns the slot and may enqueue.
    Claimed,
    /// An identical job is already queued/delayed/retrying — its id.
    Duplicate(String),
    /// The claim couldn't be made (key vanished mid-check); enqueue
    /// without dedupe rather than fail the caller.
    Unavailable,
}

/// Enqueue jobs for immediate or delayed execution.
#[derive(Clone)]
pub struct Dispatcher {
    redis: Redis,
    max_attempts: u32,
}

impl Dispatcher {
    /// Create a dispatcher over `redis` with the retry budget from
    /// `jobs`.
    pub fn new(redis: &Redis, jobs: &crate::config::JobsConfig) -> Self {
        Dispatcher {
            redis: redis.clone(),
            max_attempts: jobs.max_attempts,
        }
    }

    /// The Redis handle — for callers that need a dedupe/throttle check
    /// before enqueueing through this dispatcher.
    pub fn redis(&self) -> &Redis {
        &self.redis
    }

    /// Enqueue `kind` on its default queue.
    pub async fn enqueue(&self, kind: JobKind, payload: Value) -> Result<String> {
        self.dispatch(kind.queue(), kind, payload, None).await
    }

    /// Enqueue `kind` on `queue` explicitly.
    pub async fn enqueue_on(&self, queue: Queue, kind: JobKind, payload: Value) -> Result<String> {
        self.dispatch(queue, kind, payload, None).await
    }

    /// Enqueue `kind` to fire no earlier than `delay` from now.
    pub async fn enqueue_delayed(
        &self,
        kind: JobKind,
        payload: Value,
        delay: std::time::Duration,
    ) -> Result<String> {
        self.dispatch(kind.queue(), kind, payload, Some(delay))
            .await
    }

    /// Shared claim → push path for every enqueue variant. Remote
    /// queues first claim a dedupe slot for `(kind, payload)`; a held
    /// slot short-circuits to the existing job id. The claim is
    /// released if the push itself fails.
    async fn dispatch(
        &self,
        queue: Queue,
        kind: JobKind,
        payload: Value,
        delay: Option<std::time::Duration>,
    ) -> Result<String> {
        let mut envelope = JobEnvelope::new(kind, queue, payload, self.max_attempts);
        let dedupe_key = queue
            .is_remote()
            .then(|| remote_dedupe_key(kind, &envelope.payload));
        if let Some(key) = dedupe_key {
            match self.claim_dedupe(&key, &envelope.id).await? {
                DedupeClaim::Duplicate(id) => return Ok(id),
                DedupeClaim::Claimed => envelope.dedupe_key = Some(key),
                DedupeClaim::Unavailable => {}
            }
        }
        let id = envelope.id.clone();
        let pushed = match delay {
            None => queue::push(&self.redis, queue, &envelope).await,
            Some(delay) => {
                let run_at_ms = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64 + delay.as_millis() as u64)
                    .unwrap_or_else(|_| delay.as_millis() as u64);
                queue::push_delayed(&self.redis, &envelope, run_at_ms).await
            }
        };
        if let Err(e) = pushed {
            if let Some(key) = &envelope.dedupe_key {
                queue::release_dedupe(&self.redis, key, &id).await;
            }
            return Err(e);
        }
        Ok(id)
    }

    /// Claim `jobs:dedupe:{key}` for `job_id`. `SET NX EX` is the
    /// atomic gate; on contention the stored job id is returned so the
    /// caller can report the existing job instead of a new one. Only
    /// the "key vanished between SET NX and GET" race falls back to
    /// [`DedupeClaim::Unavailable`]; Redis errors propagate and fail
    /// the enqueue (the push would fail on the same outage anyway).
    async fn claim_dedupe(&self, key: &str, job_id: &str) -> Result<DedupeClaim> {
        let full = self.redis.key(&format!("jobs:dedupe:{key}"));
        for _ in 0..2 {
            let mut conn = self.redis.conn().await?;
            let claimed: Option<String> = redis::cmd("SET")
                .arg(&full)
                .arg(job_id)
                .arg("NX")
                .arg("EX")
                .arg(REMOTE_JOB_DEDUPE_TTL_SECONDS)
                .query_async(&mut *conn)
                .await?;
            if claimed.is_some() {
                return Ok(DedupeClaim::Claimed);
            }
            let existing: Option<String> = conn.get(&full).await?;
            if let Some(id) = existing {
                return Ok(DedupeClaim::Duplicate(id));
            }
            // The key expired between SET NX and GET — retry the claim.
        }
        Ok(DedupeClaim::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let envelope = JobEnvelope::new(
            JobKind::EmailSend,
            Queue::Default,
            serde_json::json!({"to": "user@example.com"}),
            5,
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: JobEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, envelope.id);
        assert_eq!(parsed.kind, JobKind::EmailSend);
        assert_eq!(parsed.queue, Queue::Default);
        assert_eq!(parsed.attempts, 0);
        assert_eq!(parsed.max_attempts, 5);
    }

    #[test]
    fn kind_queue_routing() {
        assert_eq!(JobKind::FederationDelivery.queue(), Queue::Federation);
        assert_eq!(JobKind::EmailSend.queue(), Queue::Default);
        assert_eq!(JobKind::NotificationFanout.queue(), Queue::Notifications);
        assert_eq!(JobKind::CleanupRemoteActivities.queue(), Queue::Cleanup);
        assert_eq!(JobKind::InstanceStatsRefresh.queue(), Queue::Cleanup);
        assert_eq!(JobKind::Export.queue(), Queue::Bulk);
        assert_eq!(JobKind::CleanupRemoteMedia.queue(), Queue::Cleanup);
    }

    #[test]
    fn remote_work_is_isolated_from_the_default_queue() {
        // User-visible remote object fetches get their own lane…
        assert_eq!(JobKind::RemoteStatusFetch.queue(), Queue::RemoteStatus);
        // …bulk remote fetches/syncs another…
        for kind in [
            JobKind::RemoteMediaFetch,
            JobKind::RemoteActorFetch,
            JobKind::RemoteActorResolve,
            JobKind::RemoteNetworkSync,
            JobKind::WebmentionIncoming,
            JobKind::WebmentionOutgoing,
        ] {
            assert_eq!(kind.queue(), Queue::Remote, "{kind:?}");
        }
        // …and large remote walks the furthest-out lane.
        assert_eq!(JobKind::ThreadPath.queue(), Queue::RemoteBulk);
        // Best-effort remote-adjacent work rides `bulk`, never `default`.
        assert_eq!(JobKind::FetchLinkPreview.queue(), Queue::Bulk);
        assert_eq!(JobKind::ThreadExpand.queue(), Queue::Bulk);
        // Nothing remote-dependent may land on `default` again.
        for kind in JobKind::ALL {
            if kind.queue() == Queue::Default {
                assert!(!kind.as_str().starts_with("remote_"), "{kind:?}");
            }
        }
    }

    #[test]
    fn publish_work_gets_its_own_lane() {
        assert_eq!(JobKind::ScheduledStatusPublish.queue(), Queue::Publish);
        assert_eq!(JobKind::QuoteAuthorization.queue(), Queue::Publish);
        assert!(Queue::ALL.contains(&Queue::Publish));
        assert_eq!(Queue::Publish.as_str(), "publish");
    }

    #[test]
    fn new_queues_are_registered() {
        for queue in [Queue::RemoteStatus, Queue::Remote, Queue::RemoteBulk] {
            assert!(Queue::ALL.contains(&queue));
            assert!(queue.is_remote());
        }
        assert_eq!(Queue::RemoteStatus.as_str(), "remote_status");
        assert_eq!(Queue::Remote.as_str(), "remote");
        assert_eq!(Queue::RemoteBulk.as_str(), "remote_bulk");
        assert!(!Queue::Default.is_remote());
        assert!(!Queue::Publish.is_remote());
    }

    #[test]
    fn remote_dedupe_key_is_payload_sensitive() {
        let resolve = remote_dedupe_key(
            JobKind::RemoteStatusFetch,
            &serde_json::json!({"uri": "https://example.com/1"}),
        );
        let refresh = remote_dedupe_key(
            JobKind::RemoteStatusFetch,
            &serde_json::json!({"uri": "https://example.com/1", "refresh": true}),
        );
        let other_uri = remote_dedupe_key(
            JobKind::RemoteStatusFetch,
            &serde_json::json!({"uri": "https://example.com/2"}),
        );
        assert_ne!(resolve, refresh);
        assert_ne!(resolve, other_uri);
        // Object key order must not change the key.
        let reordered = remote_dedupe_key(
            JobKind::RemoteStatusFetch,
            &serde_json::json!({"refresh": true, "uri": "https://example.com/1"}),
        );
        assert_eq!(refresh, reordered);
    }

    #[test]
    fn consumer_count_uses_per_queue_overrides() {
        let mut jobs = crate::config::JobsConfig {
            concurrency: 4,
            ..Default::default()
        };
        assert_eq!(Queue::Default.consumer_count(&jobs), 4);
        assert_eq!(Queue::Publish.consumer_count(&jobs), 4);
        assert_eq!(Queue::RemoteStatus.consumer_count(&jobs), 4);
        assert_eq!(Queue::Remote.consumer_count(&jobs), 8);
        assert_eq!(Queue::RemoteBulk.consumer_count(&jobs), 4);
        jobs.remote_concurrency = Some(3);
        jobs.bulk_concurrency = Some(2);
        jobs.remote_status_concurrency = Some(6);
        assert_eq!(Queue::Remote.consumer_count(&jobs), 3);
        assert_eq!(Queue::Bulk.consumer_count(&jobs), 2);
        assert_eq!(Queue::RemoteStatus.consumer_count(&jobs), 6);
    }

    #[test]
    fn timeline_fan_out_is_isolated_from_remote_fetches() {
        assert_eq!(JobKind::TimelineFanOut.queue(), Queue::Timeline);
        assert_eq!(JobKind::RemoteActorFetch.queue(), Queue::Remote);
        assert!(Queue::ALL.contains(&Queue::Timeline));
        assert_eq!(Queue::Timeline.as_str(), "timeline");
    }

    #[test]
    fn notification_fanout_is_isolated_from_remote_fetches() {
        assert_eq!(JobKind::NotificationFanout.queue(), Queue::Notifications);
        assert_eq!(JobKind::RemoteActorFetch.queue(), Queue::Remote);
        assert!(Queue::ALL.contains(&Queue::Notifications));
        assert_eq!(Queue::Notifications.as_str(), "notifications");
    }

    #[test]
    fn kind_wire_names_are_snake_case() {
        for kind in JobKind::ALL {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
    }
}
