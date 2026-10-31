//! `yonder run-job` — enqueue a background job for the worker
//! (default), or execute its production handler in-process with
//! `--inline`. Inline runs share the worker's [`build_registry`]
//! wiring but bypass retry and dead-letter accounting.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::{jobs::JobKind, Error, Result};

use super::{build_registry, context, output::Output};

/// Options every `run-job <job>` subcommand shares. The job-specific
/// doc comment above each [`JobCommand`] variant documents the payload
/// shape that `--payload` accepts.
#[derive(Debug, Args)]
pub struct JobArgs {
    /// Job payload as a JSON object (default: {}).
    #[arg(long, value_name = "JSON")]
    pub payload: Option<String>,
    /// Execute the handler in this process instead of enqueuing.
    /// Bypasses worker retry and dead-letter accounting.
    #[arg(long)]
    pub inline: bool,
    /// Enqueue the job to fire no earlier than this many seconds
    /// from now (enqueue mode only).
    #[arg(long, value_name = "SECS")]
    pub delay: Option<u64>,
}

/// One `run-job` subcommand per [`JobKind`] — the subcommand name IS
/// the wire name, so `yonder run-job --help` lists every job and
/// `yonder run-job <job> --help` documents its payload. Unknown names
/// land in `Other` so a typo still maps to the stable not-found exit
/// code instead of a clap usage error.
#[derive(Debug, Subcommand)]
pub enum JobCommand {
    /// Deliver one queued outbound activity to its remote inbox.
    ///
    /// Payload: `{"delivery_id": <deliveries row id>}` — enqueued by
    /// the delivery pipeline, rarely useful by hand.
    #[command(name = "federation_delivery")]
    FederationDelivery(JobArgs),
    /// Verify and process one queued inbound ActivityPub activity.
    ///
    /// Internal kind — the inbox route enqueues
    /// `{"activity", "path", "headers", "body_hex", "username"?}`.
    #[command(name = "federation_incoming")]
    FederationIncoming(JobArgs),
    /// Re-enqueue `deliveries` rows whose queue entry was lost.
    ///
    /// Payload: `{}` — enqueued by the scheduler and recovery paths.
    #[command(name = "federation_delivery_sweep")]
    FederationDeliverySweep(JobArgs),
    /// Fetch a remote media attachment into the local cache.
    ///
    /// Payload: `{"media_id": <media row id>}`.
    #[command(name = "remote_media_fetch")]
    RemoteMediaFetch(JobArgs),
    /// Refresh one remote actor's cached document, or sweep every
    /// stale remote actor (the `remote_actor_refresh_schedule` cron).
    ///
    /// Payload: `{"actor_url": "https://remote/users/alice"}`
    /// (`"actor_id"` accepted as an alias); `{}` runs the sweep.
    #[command(name = "remote_actor_fetch")]
    RemoteActorFetch(JobArgs),
    /// Signed-fetch and materialize a remote status object.
    ///
    /// Payload: `{"uri": "https://remote/statuses/1"}`; add
    /// `"refresh": true` to re-fetch an already-materialized row.
    #[command(name = "remote_status_fetch")]
    RemoteStatusFetch(JobArgs),
    /// Fan one notification out to its recipients.
    ///
    /// Internal kind — `{"type", "from_account_id", "account_id",
    /// "status_id"?, "report_id"?}` or a remote-interaction payload
    /// (`{"source_actor_id", "target_resource", "interaction_type"}`).
    #[command(name = "notification_fanout")]
    NotificationFanout(JobArgs),
    /// Send one email via SMTP.
    ///
    /// Internal kind (payloads may carry secrets) — template sends
    /// `{"kind", "account_id", …}`; direct sends `{"to", "subject",
    /// "text_body"/"html_body"}`; `"force": true` bypasses the class
    /// enable flag.
    #[command(name = "email_send")]
    EmailSend(JobArgs),
    /// Process one queued `archive_imports` row.
    ///
    /// Payload: `{"import_id": <row id>}` — enqueued by the import API.
    #[command(name = "import")]
    Import(JobArgs),
    /// Build the archive zip for one `archive_exports` row.
    ///
    /// Payload: `{"export_id": <row id>}` — enqueued by the export API.
    #[command(name = "export")]
    Export(JobArgs),
    /// Prune remote activities no local user has interacted with.
    ///
    /// Payload: `{}`.
    #[command(name = "cleanup_remote_activities")]
    CleanupRemoteActivities(JobArgs),
    /// Evict cached remote media past the retention window.
    ///
    /// Payload: `{}`.
    #[command(name = "cleanup_remote_media")]
    CleanupRemoteMedia(JobArgs),
    /// Re-verify `rel="me"` link-valued profile fields.
    ///
    /// Payload: `{"account_id": <id>}` for one account (local or
    /// remote); `{}` sweeps every local account.
    #[command(name = "verify_profile_links")]
    VerifyProfileLinks(JobArgs),
    /// Purge deleted account data.
    ///
    /// Payload: `{"account_id": <id>}` purges one account; `{}` sweeps
    /// every due `account_deletions` row.
    #[command(name = "account_deletion")]
    AccountDeletion(JobArgs),
    /// Migrate local followers between two accounts after a verified
    /// account move.
    ///
    /// Payload: `{"direction": "local", "origin_id", "target_id"}` or
    /// `{"direction": "remote", "origin_actor", "target_actor"}` (actor
    /// URIs); `"cursor"` resumes a batch — enqueued by the moves API.
    #[command(name = "migrate_followers")]
    MigrateFollowers(JobArgs),
    /// Delete expired archive-export files.
    ///
    /// Payload: `{}`.
    #[command(name = "export_cleanup")]
    ExportCleanup(JobArgs),
    /// Publish scheduled statuses.
    ///
    /// Payload: `{"scheduled_status_id": <id>}` publishes one row;
    /// `{}` sweeps every due row (the cron shape).
    #[command(name = "scheduled_status_publish")]
    ScheduledStatusPublish(JobArgs),
    /// Fetch Open Graph link-preview metadata.
    ///
    /// Payload: `{"status_id": <id>}` refreshes one status; `{}` fans
    /// out a rescan of statuses missing a preview card.
    #[command(name = "fetch_link_preview")]
    FetchLinkPreview(JobArgs),
    /// Tombstone statuses due for scheduled deletion.
    ///
    /// Payload: `{"status_id": <id>}` deletes one status; `{}` sweeps
    /// every due row.
    #[command(name = "status_deletion")]
    StatusDeletion(JobArgs),
    /// Delete unattached media uploads past the retention window.
    ///
    /// Payload: `{}`.
    #[command(name = "cleanup_orphan_media")]
    CleanupOrphanMedia(JobArgs),
    /// Settle one inbound `Follow`'s notification once the row has
    /// been stored.
    ///
    /// Internal kind — `{"actor_id", "target_actor_id"}` enqueued by
    /// the AP ingest.
    #[command(name = "follow_event")]
    FollowEvent(JobArgs),
    /// Fan a status out to follower home feeds, or backfill one
    /// follower's feed after a new follow.
    ///
    /// Payload: `{"status_id": <id>}` for fan-out;
    /// `{"backfill_for": <follower id>, "of": <author id>}` for
    /// backfill.
    #[command(name = "timeline_fan_out")]
    TimelineFanOut(JobArgs),
    /// Delete one stored object once nothing references it.
    ///
    /// Internal kind — `{"key": "<storage key>"}` enqueued delayed for
    /// replaced/cleared profile images.
    #[command(name = "delete_media_object")]
    DeleteMediaObject(JobArgs),
    /// Prune `audit_log` entries past
    /// `admin.audit_log_retention_days`.
    ///
    /// Payload: `{}`.
    #[command(name = "audit_log_cleanup")]
    AuditLogCleanup(JobArgs),
    /// Any argv clap didn't match to a job subcommand — resolves to
    /// the same not-found/validation errors a bare `kind` string used
    /// to produce.
    #[command(external_subcommand)]
    Other(Vec<String>),
}

impl JobCommand {
    /// The [`JobKind`] this subcommand runs plus its shared options.
    fn parts(self) -> Result<(JobKind, JobArgs)> {
        let (kind, args) = match self {
            Self::FederationDelivery(a) => (JobKind::FederationDelivery, a),
            Self::FederationIncoming(a) => (JobKind::FederationIncoming, a),
            Self::FederationDeliverySweep(a) => (JobKind::FederationDeliverySweep, a),
            Self::RemoteMediaFetch(a) => (JobKind::RemoteMediaFetch, a),
            Self::RemoteActorFetch(a) => (JobKind::RemoteActorFetch, a),
            Self::RemoteStatusFetch(a) => (JobKind::RemoteStatusFetch, a),
            Self::NotificationFanout(a) => (JobKind::NotificationFanout, a),
            Self::EmailSend(a) => (JobKind::EmailSend, a),
            Self::Import(a) => (JobKind::Import, a),
            Self::Export(a) => (JobKind::Export, a),
            Self::CleanupRemoteActivities(a) => (JobKind::CleanupRemoteActivities, a),
            Self::CleanupRemoteMedia(a) => (JobKind::CleanupRemoteMedia, a),
            Self::VerifyProfileLinks(a) => (JobKind::VerifyProfileLinks, a),
            Self::AccountDeletion(a) => (JobKind::AccountDeletion, a),
            Self::MigrateFollowers(a) => (JobKind::MigrateFollowers, a),
            Self::ExportCleanup(a) => (JobKind::ExportCleanup, a),
            Self::ScheduledStatusPublish(a) => (JobKind::ScheduledStatusPublish, a),
            Self::FetchLinkPreview(a) => (JobKind::FetchLinkPreview, a),
            Self::StatusDeletion(a) => (JobKind::StatusDeletion, a),
            Self::CleanupOrphanMedia(a) => (JobKind::CleanupOrphanMedia, a),
            Self::FollowEvent(a) => (JobKind::FollowEvent, a),
            Self::TimelineFanOut(a) => (JobKind::TimelineFanOut, a),
            Self::DeleteMediaObject(a) => (JobKind::DeleteMediaObject, a),
            Self::AuditLogCleanup(a) => (JobKind::AuditLogCleanup, a),
            Self::Other(argv) => return Err(unknown_kind(&argv)),
        };
        Ok((kind, args))
    }
}

/// Resolve `name` to a [`JobKind`] by its wire name,
/// case-insensitively. Unknown kinds list the valid names.
pub fn resolve_kind(name: &str) -> Result<JobKind> {
    JobKind::ALL
        .iter()
        .copied()
        .find(|k| k.as_str().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let kinds = JobKind::ALL
                .iter()
                .map(JobKind::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            Error::NotFound(format!("job kind {name:?} (valid kinds: {kinds})"))
        })
}

/// Error for argv that matched no job subcommand: a typo is the
/// not-found (exit 3) valid-kinds list; a case-mismatched real name
/// points at the canonical spelling since subcommands are
/// case-sensitive.
fn unknown_kind(argv: &[String]) -> Error {
    let name = argv.first().map(String::as_str).unwrap_or_default();
    match resolve_kind(name) {
        Ok(kind) => Error::Validation(format!(
            "unknown job {name:?} — did you mean {:?}?",
            kind.as_str()
        )),
        Err(e) => e,
    }
}

/// Dispatch a parsed `run-job` invocation.
pub async fn dispatch(path: Option<&Path>, job: JobCommand, out: &Output) -> Result<u8> {
    let (kind, args) = job.parts()?;
    run_job(
        path,
        kind,
        args.payload.as_deref(),
        args.inline,
        args.delay,
        out,
    )
    .await
}

pub async fn run_job(
    path: Option<&Path>,
    kind: JobKind,
    payload: Option<&str>,
    inline: bool,
    delay: Option<u64>,
    out: &Output,
) -> Result<u8> {
    let payload: Value = match payload {
        None => json!({}),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| Error::Validation(format!("--payload is not valid JSON: {e}")))?,
    };
    if inline && delay.is_some() {
        return Err(Error::Validation(
            "--delay applies to enqueued jobs only".to_string(),
        ));
    }

    if inline {
        run_inline(path, kind, &payload, out).await
    } else {
        enqueue(path, kind, payload, delay, out).await
    }
}

async fn enqueue(
    path: Option<&Path>,
    kind: JobKind,
    payload: Value,
    delay: Option<u64>,
    out: &Output,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let id = match delay {
        None => ctx.state.dispatcher.enqueue(kind, payload).await?,
        Some(secs) => {
            // `enqueue_delayed` adds the current epoch to the delay
            // itself — reject delays that would overflow the u64
            // millisecond timestamp rather than wrap.
            let delay = Duration::from_secs(secs);
            let delay_ms = u64::try_from(delay.as_millis())
                .map_err(|_| Error::Validation("--delay is too large".to_string()))?;
            let now_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now_ms.checked_add(delay_ms).ok_or_else(|| {
                Error::Validation("--delay is too large (timestamp overflow)".to_string())
            })?;
            ctx.state
                .dispatcher
                .enqueue_delayed(kind, payload, delay)
                .await?
        }
    };
    let queue = kind.queue().as_str();
    if out.json_mode() {
        out.emit_json(&json!({"id": id, "queue": queue, "inline": false}))?;
    } else {
        out.emit_human(&format!("enqueued {id} on queue {queue}"))?;
    }
    Ok(0)
}

async fn run_inline(
    path: Option<&Path>,
    kind: JobKind,
    payload: &Value,
    out: &Output,
) -> Result<u8> {
    let ctx = context::load_runtime(path, true).await?;
    let registry = build_registry(ctx.ap.as_ref());
    let handler = registry
        .handler(kind)
        .ok_or_else(|| Error::NotFound(format!("no handler registered for {}", kind.as_str())))?;
    let job_ctx = crate::jobs::JobContext {
        db: ctx.pool.clone(),
        redis: ctx.redis.clone(),
        config: Arc::new(ctx.config.clone()),
        media: crate::media::storage::MediaStore::from_config(&ctx.config),
    };
    out.warn("running job inline (bypasses worker retry/dead-letter accounting)");
    handler.run(&job_ctx, payload).await?;
    if out.json_mode() {
        out.emit_json(&json!({"inline": true, "ok": true}))?;
    } else {
        out.emit_human("job completed")?;
    }
    Ok(0)
}
