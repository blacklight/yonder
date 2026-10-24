//! Moderation audit log — the single writer for `audit_log` (epic 21)
//! plus the retention sweep. The reader endpoint is epic 22's
//! (`GET /api/v1/admin/audit_log`); this module only produces rows.
//!
//! Every moderation mutation calls [`record`] exactly once — reports
//! resolve/reopen/assign, account actions, role changes and
//! domain-block writes. `details` carries the full parameter set so
//! the admin panel can render the change without schema changes.

use serde_json::{json, Value};

use crate::jobs::{JobContext, JobHandler};
use crate::Result;

/// Write one `audit_log` row.
///
/// `db` is any sqlx executor — pass `&state.db` directly or
/// `&mut *tx` to commit the row atomically with the mutation it
/// describes.
///
/// - `actor_account_id`: the admin/moderator performing the action,
///   `NULL` for system-driven entries.
/// - `action`: dotted verb, e.g. `report.resolve`, `account.suspend`,
///   `domain_block.create`, `account.role_change`.
/// - `target_type`/`target_id`: what the action hit (`report`,
///   `account`, `domain`), `NULL` when not row-shaped.
/// - `details`: free-form context (parameters, reason, prior state).
pub async fn record<'e, E>(
    db: E,
    actor_account_id: Option<i64>,
    action: &str,
    target_type: &str,
    target_id: Option<i64>,
    details: Value,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO audit_log (actor_account_id, action, target_type, target_id, details) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor_account_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(details)
    .execute(db)
    .await?;
    Ok(())
}

/// `JobKind::AuditLogCleanup` — delete `audit_log` rows older than
/// `admin.audit_log_retention_days`. Scheduled via
/// `admin.audit_log_sweep_schedule`. Payload is ignored; the config
/// value at run time governs.
pub struct AuditLogCleanupJob;

#[async_trait::async_trait]
impl JobHandler for AuditLogCleanupJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let days = i64::from(ctx.config.admin.audit_log_retention_days);
        let deleted: u64 = sqlx::query(
            "DELETE FROM audit_log WHERE created_at < now() - $1::bigint * interval '1 day'",
        )
        .bind(days)
        .execute(&ctx.db)
        .await?
        .rows_affected();
        if deleted > 0 {
            tracing::info!(deleted, days, "audit log retention sweep pruned entries");
        }
        Ok(())
    }
}

/// Convenience for the common "one moderation mutation, one audit
/// row" shape — builds the details object and records in one call.
/// Takes the same executors as [`record`] (`&pool` or `&mut *tx`).
pub async fn record_action<'e, E>(
    db: E,
    actor_account_id: i64,
    action: &str,
    target_type: &str,
    target_id: i64,
    reason: &str,
    extra: Value,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let details = json!({
        "reason": reason,
        "params": extra,
    });
    record(
        db,
        Some(actor_account_id),
        action,
        target_type,
        Some(target_id),
        details,
    )
    .await
}
