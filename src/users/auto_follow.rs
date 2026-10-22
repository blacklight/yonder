//! Config-driven auto-follow (epic 11): `admin.auto_follow_accounts`
//! are accounts every newly active user follows, and
//! `admin.auto_followed_by` are **local** accounts that follow every
//! newly active user.
//!
//! [`apply`] runs when an account becomes active: directly after an
//! `open` registration (and after an invite auto-approval), and at
//! approval time under `approval-required` (a consumer contract for
//! epic 22). All failures are per-entry and logged — auto-follow must
//! never fail or roll back a registration.

use sqlx::PgPool;

use crate::{
    activitypub::ApContext,
    config::Config,
    users::relationships::{self, Ctx, FollowOpts},
};

/// Owned inputs [`apply`] needs — cloneable into a `tokio::spawn`ed
/// fire-and-forget task from the registration path.
#[derive(Clone)]
pub struct AutoFollowCtx {
    /// PostgreSQL pool.
    pub db: PgPool,
    /// Effective configuration.
    pub config: std::sync::Arc<Config>,
    /// Federation context, when enabled.
    pub ap: Option<ApContext>,
}

/// Apply the configured auto-follow rules for `account_id`. Best-effort:
/// unknown nicknames, remote `auto_followed_by` entries (we cannot make
/// a remote actor follow anyone) and per-row resolution failures log
/// and are skipped.
pub async fn apply(ctx: &AutoFollowCtx, account_id: i64) -> crate::Result<()> {
    let rel = Ctx {
        db: &ctx.db,
        config: &ctx.config,
        ap: ctx.ap.as_ref(),
    };
    for entry in &ctx.config.admin.auto_follow_accounts {
        match relationships::resolve_account(rel, entry).await {
            Ok(target_id) if target_id != account_id => {
                if let Err(e) =
                    relationships::follow(rel, account_id, target_id, FollowOpts::default()).await
                {
                    tracing::warn!(entry, error = %e, "auto-follow failed");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(entry, error = %e, "auto-follow account unresolved");
            }
        }
    }
    for entry in &ctx.config.admin.auto_followed_by {
        match relationships::resolve_account(rel, entry).await {
            Ok(source_id) => {
                // Only local accounts can be made to follow someone —
                // a remote entry would fabricate a one-sided follow
                // the remote knows nothing about.
                let local: bool = sqlx::query_scalar(
                    "SELECT domain IS NULL AND suspended_at IS NULL AND deleted_at IS NULL \
                     FROM accounts WHERE id = $1",
                )
                .bind(source_id)
                .fetch_one(&ctx.db)
                .await
                .unwrap_or(false);
                if !local {
                    tracing::warn!(
                        entry,
                        "auto_followed_by entry is not a usable local account"
                    );
                    continue;
                }
                if let Err(e) =
                    relationships::follow(rel, source_id, account_id, FollowOpts::default()).await
                {
                    tracing::warn!(entry, error = %e, "auto-followed-by failed");
                }
            }
            Err(e) => {
                tracing::warn!(entry, error = %e, "auto_followed_by account unresolved");
            }
        }
    }
    Ok(())
}
