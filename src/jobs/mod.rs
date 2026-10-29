//! Background-job queue, task dispatcher and scheduler.
//!
//! Jobs persist in Redis Streams so they survive process restarts;
//! duplicate delivery is tolerated, so **handlers must be idempotent**.
//!
//! - [`JobKind`] — the typed registry of job kinds, each routed to a
//!   [`Queue`] (`default` / `federation` / `bulk`, mirroring Songhive's
//!   queue taxonomy).
//! - [`JobHandler`] — implement it and [`Registry::register`] it to plug
//!   a real implementation in (later epics); [`stub`] registers
//!   placeholder handlers so the machinery works end-to-end today.
//! - [`Dispatcher`] — enqueue jobs for immediate or delayed execution.
//! - [`worker::Worker`] — claims, executes, acks/nacks with bounded
//!   retries, exponential backoff and dead-lettering.
//! - [`scheduler`] — cron schedules (from config) that enqueue the
//!   cleanup job kinds.

pub mod queue;
pub mod scheduler;
pub mod stub;
pub mod worker;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::{cache::Redis, config::Config, Result};

/// Queue names — one Redis stream each.
///
/// - `default`: interactive / low-volume work (email, notifications,
///   remote fetches).
/// - `federation`: latency-sensitive delivery — a remote inbox answer
///   minutes late is worthless, so it never waits behind bulk work.
/// - `bulk`: high-volume per-item jobs (imports, exports, cleanup
///   fan-out) that would otherwise starve everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Queue {
    /// Interactive, low-volume jobs.
    Default,
    /// Latency-sensitive federation delivery.
    Federation,
    /// High-volume per-item jobs.
    Bulk,
    /// Scheduled cleanup sweeps — kept off `bulk` so a long-running
    /// cleanup can't starve per-item fan-out work.
    Cleanup,
}

impl Queue {
    /// Every queue the worker drains.
    pub const ALL: &'static [Queue] = &[
        Queue::Default,
        Queue::Federation,
        Queue::Bulk,
        Queue::Cleanup,
    ];

    /// Stream suffix: `default` / `federation` / `bulk`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Queue::Default => "default",
            Queue::Federation => "federation",
            Queue::Bulk => "bulk",
            Queue::Cleanup => "cleanup",
        }
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
    /// Fetch and materialize a remote status object (`{"uri": …}`) —
    /// resolves pending quote targets whose servers never federate
    /// with us.
    RemoteStatusFetch,
    /// Fan a notification out to its recipients.
    NotificationFanout,
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
    /// Settle an inbound `Follow`'s notification once pubby's dispatch
    /// has stored the row — emits `follow` or `follow_request`
    /// depending on which row landed (epic 14).
    FollowEvent,
    /// Fan a status out to follower home feeds, or backfill one
    /// follower's feed from a new followee (epic 15).
    TimelineFanOut,
    /// Delete one stored object once nothing references it — enqueued
    /// delayed for replaced/cleared profile images so the old URL
    /// survives the actor-`Update` propagation window (epic 18).
    DeleteMediaObject,
    /// Prune `audit_log` entries past `admin.audit_log_retention_days`
    /// (epic 21, `admin.audit_log_sweep_schedule`).
    AuditLogCleanup,
}

impl JobKind {
    /// Every known job kind.
    pub const ALL: &'static [JobKind] = &[
        JobKind::FederationDelivery,
        JobKind::FederationIncoming,
        JobKind::FederationDeliverySweep,
        JobKind::RemoteMediaFetch,
        JobKind::RemoteActorFetch,
        JobKind::RemoteStatusFetch,
        JobKind::NotificationFanout,
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
        JobKind::FollowEvent,
        JobKind::TimelineFanOut,
        JobKind::DeleteMediaObject,
        JobKind::AuditLogCleanup,
    ];

    /// Stable wire name (`snake_case`).
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::FederationDelivery => "federation_delivery",
            JobKind::FederationIncoming => "federation_incoming",
            JobKind::FederationDeliverySweep => "federation_delivery_sweep",
            JobKind::RemoteMediaFetch => "remote_media_fetch",
            JobKind::RemoteActorFetch => "remote_actor_fetch",
            JobKind::RemoteStatusFetch => "remote_status_fetch",
            JobKind::NotificationFanout => "notification_fanout",
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
            JobKind::FollowEvent => "follow_event",
            JobKind::TimelineFanOut => "timeline_fan_out",
            JobKind::DeleteMediaObject => "delete_media_object",
            JobKind::AuditLogCleanup => "audit_log_cleanup",
        }
    }

    /// The queue this kind routes to by default.
    pub fn queue(&self) -> Queue {
        match self {
            JobKind::FederationDelivery
            | JobKind::FederationIncoming
            | JobKind::FederationDeliverySweep => Queue::Federation,
            JobKind::Import
            | JobKind::Export
            | JobKind::VerifyProfileLinks
            | JobKind::MigrateFollowers
            | JobKind::StatusDeletion => Queue::Bulk,
            JobKind::CleanupRemoteActivities
            | JobKind::CleanupRemoteMedia
            | JobKind::AccountDeletion
            | JobKind::ExportCleanup
            | JobKind::CleanupOrphanMedia
            | JobKind::AuditLogCleanup
            | JobKind::DeleteMediaObject => Queue::Cleanup,
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

    /// Enqueue `kind` on its default queue.
    pub async fn enqueue(&self, kind: JobKind, payload: Value) -> Result<String> {
        self.enqueue_on(kind.queue(), kind, payload).await
    }

    /// Enqueue `kind` on `queue` explicitly.
    pub async fn enqueue_on(&self, queue: Queue, kind: JobKind, payload: Value) -> Result<String> {
        let envelope = JobEnvelope::new(kind, queue, payload, self.max_attempts);
        let id = envelope.id.clone();
        queue::push(&self.redis, queue, &envelope).await?;
        Ok(id)
    }

    /// Enqueue `kind` to fire no earlier than `delay` from now.
    pub async fn enqueue_delayed(
        &self,
        kind: JobKind,
        payload: Value,
        delay: std::time::Duration,
    ) -> Result<String> {
        let envelope = JobEnvelope::new(kind, kind.queue(), payload, self.max_attempts);
        let id = envelope.id.clone();
        let run_at_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 + delay.as_millis() as u64)
            .unwrap_or_else(|_| delay.as_millis() as u64);
        queue::push_delayed(&self.redis, &envelope, run_at_ms).await?;
        Ok(id)
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
        assert_eq!(JobKind::NotificationFanout.queue(), Queue::Default);
        assert_eq!(JobKind::CleanupRemoteActivities.queue(), Queue::Cleanup);
        assert_eq!(JobKind::Export.queue(), Queue::Bulk);
        assert_eq!(JobKind::CleanupRemoteMedia.queue(), Queue::Cleanup);
    }

    #[test]
    fn kind_wire_names_are_snake_case() {
        for kind in JobKind::ALL {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
    }
}
