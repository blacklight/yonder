//! Cron scheduling on top of the job queue.
//!
//! Scheduled jobs *enqueue* through the [`Dispatcher`] — the scheduler
//! only triggers, the worker executes, so cron jobs share the same
//! retry/dead-letter semantics as every other job. Schedules come from
//! the admin-configurable keys `federation.remote_activity_prune_schedule`
//! and `media.remote_cleanup_schedule` (5-field crontab syntax).
//!
//! The scheduler is in-process and registered from config at startup, so
//! nothing needs to persist across restarts — run it wherever the
//! worker runs, once per deployment.

use serde_json::json;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::{
    config::Config,
    jobs::{Dispatcher, JobKind},
    Error, Result,
};

/// Translate a 5-field crontab expression (`min hour dom mon dow`) to
/// the 6-field form tokio-cron-scheduler expects (seconds first).
/// 6+ field expressions pass through unchanged.
pub fn to_cron(expression: &str) -> Result<String> {
    let fields = expression.split_whitespace().count();
    match fields {
        5 => Ok(format!("0 {}", expression.trim())),
        6 | 7 => Ok(expression.trim().to_string()),
        _ => Err(Error::Validation(format!(
            "invalid cron expression {expression:?} — expected 5 or 6 fields"
        ))),
    }
}

/// Register the configured cleanup schedules and start the scheduler.
///
/// Returns the running [`JobScheduler`] — keep it alive for the lifetime
/// of the worker; dropping it stops future firings.
pub async fn start(config: &Config, dispatcher: &Dispatcher) -> Result<JobScheduler> {
    let scheduler = JobScheduler::new()
        .await
        .map_err(|e| Error::Internal(e.to_string()))?;
    let schedules = [
        (
            config.federation.remote_activity_prune_schedule.as_deref(),
            JobKind::CleanupRemoteActivities,
        ),
        (
            config.media.remote_cleanup_schedule.as_deref(),
            JobKind::CleanupRemoteMedia,
        ),
        (
            config
                .federation
                .profile_link_verification_schedule
                .as_deref(),
            JobKind::VerifyProfileLinks,
        ),
        // Only meaningful with the federation stack up — the ledger
        // this reconciles doesn't exist otherwise.
        (
            if config.federation.enabled {
                config.federation.delivery_sweep_schedule.as_deref()
            } else {
                None
            },
            JobKind::FederationDeliverySweep,
        ),
        (
            config.admin.account_deletion_sweep_schedule.as_deref(),
            JobKind::AccountDeletion,
        ),
        (
            config.admin.export_cleanup_schedule.as_deref(),
            JobKind::ExportCleanup,
        ),
        // Epic-12 posting sweeps.
        (
            config.content.scheduled_status_sweep_schedule.as_deref(),
            JobKind::ScheduledStatusPublish,
        ),
        (
            config.content.scheduled_deletion_sweep_schedule.as_deref(),
            JobKind::StatusDeletion,
        ),
        (
            config.media.orphan_cleanup_schedule.as_deref(),
            JobKind::CleanupOrphanMedia,
        ),
    ];
    for (expression, kind) in schedules {
        let Some(expression) = expression else {
            continue;
        };
        // An empty/blank expression is the documented "disable" value.
        if expression.trim().is_empty() {
            continue;
        }
        let cron = match to_cron(expression) {
            Ok(cron) => cron,
            // A bad config value shouldn't take the worker down — skip
            // the schedule and log loudly.
            Err(e) => {
                tracing::error!(schedule = %expression, kind = kind.as_str(), error = %e,
                    "invalid cron schedule — skipping");
                continue;
            }
        };
        let dispatcher = dispatcher.clone();
        let job = match Job::new_async(cron, move |_id, _lock| {
            let dispatcher = dispatcher.clone();
            Box::pin(async move {
                if let Err(e) = dispatcher.enqueue(kind, json!({})).await {
                    tracing::warn!(kind = kind.as_str(), error = %e, "scheduled enqueue failed");
                }
            })
        }) {
            Ok(job) => job,
            Err(e) => {
                tracing::error!(schedule = %expression, kind = kind.as_str(), error = %e,
                    "cron schedule rejected — skipping");
                continue;
            }
        };
        scheduler
            .add(job)
            .await
            .map_err(|e| Error::Internal(e.to_string()))?;
        tracing::info!(schedule = %expression, kind = kind.as_str(), "registered scheduled job");
    }
    scheduler
        .start()
        .await
        .map_err(|e| Error::Internal(e.to_string()))?;
    Ok(scheduler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_cron_is_translated() {
        assert_eq!(to_cron("0 3 * * *").unwrap(), "0 0 3 * * *");
        assert_eq!(to_cron("*/15 * * * *").unwrap(), "0 */15 * * * *");
    }

    #[test]
    fn six_field_cron_passes_through() {
        assert_eq!(to_cron("30 0 3 * * *").unwrap(), "30 0 3 * * *");
    }

    #[test]
    fn bad_cron_is_rejected() {
        assert!(to_cron("* * *").is_err());
        assert!(to_cron("").is_err());
    }
}
