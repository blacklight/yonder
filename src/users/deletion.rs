//! Account deletion (epic 11): self-service and admin-initiated
//! scheduling, immediate deactivation + credential revocation, a
//! federated `Delete(actor)` and the batched purge — driven by
//! `account_deletions` rows and the `AccountDeletion` job so the work
//! survives restarts and Redis loss.
//!
//! Deletion keeps a **tombstone** `accounts` row: the username stays
//! reserved (registration's pre-check already counts it), and the key
//! pair remains so in-flight `Delete` deliveries can still sign.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{
    activitypub::{actors, delivery, ApContext},
    http::AppState,
    jobs::{JobContext, JobHandler, JobKind},
    Error, Result,
};

/// Rows a single sweep run claims and purges.
const SWEEP_BATCH: i64 = 50;

/// Revoke every access path for `account_id`: OAuth access and
/// refresh tokens, interactive sessions and app authorizations.
/// Runs inside `tx` so callers compose it atomically with the state
/// change motivating it (suspension, freeze, scheduled deletion).
/// Shared with epic 21's moderation actions — the auth middleware
/// rejects disabled/suspended accounts anyway, but revoked rows make
/// the revocation durable rather than check-dependent.
pub async fn revoke_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE oauth_access_tokens SET revoked_at = now() \
         WHERE revoked_at IS NULL AND user_id = (SELECT id FROM users WHERE account_id = $1)",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = now() \
         WHERE revoked_at IS NULL AND access_token_id IN ( \
             SELECT t.id FROM oauth_access_tokens t \
             JOIN users u ON u.id = t.user_id WHERE u.account_id = $1)",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE account_id = $1)",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM oauth_authorizations WHERE user_id = \
         (SELECT id FROM users WHERE account_id = $1)",
    )
    .bind(account_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// What [`schedule_account_deletion`] / [`schedule_account_deletion_cli`]
/// recorded: the `account_deletions` row id and its due time.
#[derive(Debug)]
pub struct DeletionSchedule {
    pub id: i64,
    pub scheduled_at: chrono::DateTime<chrono::Utc>,
}

/// Schedule `account_id` for deletion: deactivate the account now
/// (suspend + revoke every token/session), then record the due time.
/// `delay == 0` enqueues the purge immediately; otherwise the cron
/// sweep picks the row up once `scheduled_at` passes. Idempotent — a
/// second request returns the pending row.
///
/// `requested_by` is `None` for self-service and the admin/moderator
/// account for moderator-initiated deletions (epics 21/22).
pub async fn schedule_account_deletion(
    state: &AppState,
    account_id: i64,
    requested_by: Option<i64>,
    delay: std::time::Duration,
) -> Result<i64> {
    Ok(schedule_inner(
        state,
        account_id,
        requested_by,
        delay,
        delay.is_zero(),
        None,
    )
    .await?
    .id)
}

/// The CLI's scheduling entry point: same guards and mutation as
/// [`schedule_account_deletion`], plus an `account.delete_request`
/// audit row committed in the same transaction. `enqueue_immediate`
/// replaces the implicit `delay == 0` enqueue — `--now` passes
/// `false` so the CLI's inline purge is the only purger (a queued
/// job and the CLI would otherwise race to claim the row).
pub async fn schedule_account_deletion_cli(
    state: &AppState,
    account_id: i64,
    actor_id: i64,
    delay: std::time::Duration,
    enqueue_immediate: bool,
) -> Result<DeletionSchedule> {
    schedule_inner(
        state,
        account_id,
        Some(actor_id),
        delay,
        enqueue_immediate,
        Some(actor_id),
    )
    .await
}

/// Shared body of the two scheduling entry points. `audit_actor`
/// controls the audit row: `None` (the HTTP callers) keeps the
/// historical no-audit behavior.
async fn schedule_inner(
    state: &AppState,
    account_id: i64,
    requested_by: Option<i64>,
    delay: std::time::Duration,
    enqueue_immediate: bool,
    audit_actor: Option<i64>,
) -> Result<DeletionSchedule> {
    // The instance actor must never be deleted; nor can a deleted
    // account be scheduled twice.
    let guard: Option<(String, bool, bool, bool)> = sqlx::query_as(
        "SELECT username, domain IS NULL, deleted_at IS NULL, suspended_at IS NOT NULL \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((username, is_local, alive, was_suspended)) = guard else {
        return Err(Error::NotFound("account".into()));
    };
    if !is_local {
        return Err(Error::Validation(
            "only local accounts can be deleted".into(),
        ));
    }
    if username == actors::INSTANCE_USERNAME {
        return Err(Error::Validation(
            "the instance actor cannot be deleted".into(),
        ));
    }
    if !alive {
        return Err(Error::Validation("account is already deleted".into()));
    }

    let mut tx = state.db.begin().await?;
    // The UNIQUE(account_id) makes this atomic with concurrent
    // requests; a stale 'cancelled'/'completed' row is resurrected in
    // place ('completed' is unreachable — the deleted_at guard above
    // already refused the account).
    let scheduled: Option<(i64, DateTime<Utc>)> = sqlx::query_as(
        "INSERT INTO account_deletions \
             (account_id, requested_by_account_id, scheduled_at, was_suspended) \
         VALUES ($1, $2, now() + $3::float8 * interval '1 second', $4) \
         ON CONFLICT (account_id) DO UPDATE SET \
             requested_by_account_id = EXCLUDED.requested_by_account_id, \
             scheduled_at = EXCLUDED.scheduled_at, \
             was_suspended = EXCLUDED.was_suspended, \
             state = 'pending', updated_at = now() \
         WHERE account_deletions.state = 'cancelled' \
         RETURNING id, scheduled_at",
    )
    .bind(account_id)
    .bind(requested_by)
    .bind(delay.as_secs_f64())
    .bind(was_suspended)
    .fetch_optional(&mut *tx)
    .await?;
    let (deletion_id, scheduled_at) = match scheduled {
        Some(row) => row,
        None => {
            // Conflict with a 'pending'/'running' row: return the
            // pending row (idempotent), refuse a running purge.
            match sqlx::query_as::<_, (i64, DateTime<Utc>)>(
                "SELECT id, scheduled_at FROM account_deletions \
                 WHERE account_id = $1 AND state = 'pending'",
            )
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?
            {
                Some(row) => row,
                None => {
                    return Err(Error::Conflict(
                        "a deletion for this account is already in progress".into(),
                    ))
                }
            }
        }
    };
    // Deactivate immediately — nothing has federated yet. Suspension
    // makes the auth middleware reject every token, and revoking the
    // rows themselves makes the deactivation durable rather than
    // dependent on the suspended_at check.
    sqlx::query("UPDATE accounts SET suspended_at = now(), updated_at = now() WHERE id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    revoke_access(&mut tx, account_id).await?;
    if let Some(actor_id) = audit_actor {
        crate::moderation::audit::record(
            &mut *tx,
            Some(actor_id),
            "account.delete_request",
            "account",
            Some(account_id),
            json!({ "scheduled_at": scheduled_at.to_rfc3339() }),
        )
        .await?;
    }
    tx.commit().await?;

    if enqueue_immediate {
        state
            .dispatcher
            .enqueue(
                JobKind::AccountDeletion,
                json!({ "account_id": account_id }),
            )
            .await?;
    }
    Ok(DeletionSchedule {
        id: deletion_id,
        scheduled_at,
    })
}

/// Cancel a pending deletion: marks the row cancelled and lifts the
/// suspension. Only meaningful while `state = 'pending'` — after the
/// purge starts the account is gone remotely. A deactivated user
/// cannot authenticate, so this is an admin/CLI surface, not a route.
pub async fn cancel_account_deletion(db: &PgPool, account_id: i64) -> Result<()> {
    let was_suspended: Option<bool> = sqlx::query_scalar(
        "UPDATE account_deletions SET state = 'cancelled', updated_at = now() \
         WHERE account_id = $1 AND state = 'pending' RETURNING was_suspended",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let Some(was_suspended) = was_suspended else {
        return Err(Error::Conflict(
            "no pending deletion for this account".into(),
        ));
    };
    // Lift the deactivation only when scheduling it was what suspended
    // the account — a pre-existing moderator suspension must survive.
    if !was_suspended {
        sqlx::query(
            "UPDATE accounts SET suspended_at = NULL, updated_at = now() \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(account_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// `JobKind::AccountDeletion` — with `{account_id}` purges one account;
/// with an empty payload sweeps every due `account_deletions` row (the
/// cron path). The AP context is needed to emit `Delete` — without it
/// (federation off) the purge still runs locally.
pub struct AccountDeletionJob {
    ap: Option<ApContext>,
}

impl AccountDeletionJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        AccountDeletionJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for AccountDeletionJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        match payload.get("account_id").and_then(Value::as_i64) {
            Some(account_id) => purge_account(ctx, self.ap.as_ref(), account_id).await,
            None => {
                // Sweep: claim a bounded batch of due rows, then purge
                // each. Rows claimed to 'running' but never finished
                // (crash) are re-claimed on the next sweep — the purge
                // is idempotent.
                let ids: Vec<i64> = sqlx::query_scalar(
                    "UPDATE account_deletions SET state = 'running', updated_at = now() \
                     WHERE account_id IN ( \
                         SELECT account_id FROM account_deletions \
                         WHERE state IN ('pending','running') AND scheduled_at <= now() \
                         ORDER BY scheduled_at LIMIT $1 \
                     ) AND state IN ('pending','running') \
                     RETURNING account_id",
                )
                .bind(SWEEP_BATCH)
                .fetch_all(&ctx.db)
                .await?;
                for account_id in ids {
                    if let Err(e) = purge_account(ctx, self.ap.as_ref(), account_id).await {
                        // Leave the row 'running' — the next sweep
                        // re-claims it rather than dead-lettering.
                        tracing::error!(account_id, error = %e, "account purge failed");
                    }
                }
                Ok(())
            }
        }
    }
}

/// Emit `Delete(actor)` to followers and to the shared inboxes of
/// remote accounts the user followed or had pending requests to —
/// Mastodon delivers account deletes to every known inbox, not just
/// followers. Deterministic activity id makes re-runs no-ops through
/// the delivery ledger's unique constraint.
async fn emit_delete(ap: &ApContext, username: &str, actor_id: &str) -> Result<()> {
    let followers_url = format!("{actor_id}/followers");
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor_id}#delete"),
        "type": "Delete",
        "actor": actor_id,
        "object": { "id": actor_id, "type": "Tombstone" },
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [followers_url],
    });
    delivery::publish(ap, username, &activity).await?;

    // Extra inboxes: remote accounts this user follows or requested.
    let rows = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT DISTINCT COALESCE(NULLIF(a.shared_inbox,''), NULLIF(a.inbox,''), NULL) AS shared, \
                NULLIF(a.inbox,'') AS inbox \
         FROM accounts a \
         JOIN (SELECT target_account_id FROM follows f JOIN accounts s ON s.id = f.account_id \
               WHERE s.actor_id = $1 \
               UNION \
               SELECT target_account_id FROM follow_requests r JOIN accounts s ON s.id = r.account_id \
               WHERE s.actor_id = $1) t ON t.target_account_id = a.id \
         WHERE a.domain IS NOT NULL",
    )
    .bind(actor_id)
    .fetch_all(&ap.core.db)
    .await?;
    for (shared, inbox) in rows {
        let url = match shared.or(inbox) {
            Some(u) if !u.is_empty() => u,
            _ => continue,
        };
        if let Err(e) = delivery::enqueue_delivery(&ap.core, &url, &activity).await {
            tracing::warn!(error = %e, inbox = %url, "delete delivery enqueue failed");
        }
    }
    Ok(())
}

/// The purge itself: `Delete` → tombstone → content/relationship
/// removal → media bytes. Idempotent and resumable — every step is
/// safe to re-run.
pub async fn purge_account(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    account_id: i64,
) -> Result<()> {
    // Claim: only pending/running rows may proceed (a 'cancelled' row
    // or a missing row ends the job as a no-op).
    let claimed = sqlx::query(
        "UPDATE account_deletions SET state = 'running', updated_at = now() \
         WHERE account_id = $1 AND state IN ('pending','running') AND scheduled_at <= now()",
    )
    .bind(account_id)
    .execute(&ctx.db)
    .await?
    .rows_affected()
        > 0;
    // A completed tombstone still lets a re-delivered job finish
    // bookkeeping (e.g. the row re-inserted by a weird admin path is
    // impossible — UNIQUE — so this is purely the claim check).
    if !claimed {
        let done: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM account_deletions \
             WHERE account_id = $1 AND state = 'completed')",
        )
        .bind(account_id)
        .fetch_one(&ctx.db)
        .await?;
        if !done {
            return Ok(());
        }
    }

    let row = sqlx::query("SELECT username, actor_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&ctx.db)
        .await?;
    let Some(row) = row else {
        sqlx::query(
            "UPDATE account_deletions SET state = 'completed', updated_at = now() \
             WHERE account_id = $1",
        )
        .bind(account_id)
        .execute(&ctx.db)
        .await?;
        return Ok(());
    };
    let username: String = row.get("username");
    let actor_id: String = row.get("actor_id");

    // Emit Delete while the actor still resolves.
    if let Some(ap) = ap {
        let already: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM activities WHERE activity_id = $1 AND local)",
        )
        .bind(format!("{actor_id}#delete"))
        .fetch_one(&ctx.db)
        .await?;
        if !already {
            if let Err(e) = emit_delete(ap, &username, &actor_id).await {
                tracing::error!(account_id, error = %e, "Delete(actor) publish failed");
            }
        }
    }

    // Tombstone: redact the profile, keep username/actor_id/keys.
    sqlx::query(
        "UPDATE accounts SET deleted_at = now(), display_name = NULL, bio = NULL, \
                profile_fields = '[]'::jsonb, avatar_path = NULL, banner_path = NULL, \
                avatar_remote_url = NULL, banner_remote_url = NULL, avatar_blurhash = NULL, \
                header_blurhash = NULL, locked = false, discoverable = false, \
                directory_opt_in = false, moved_to_account_id = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(account_id)
    .execute(&ctx.db)
    .await?;
    sqlx::query("DELETE FROM account_aliases WHERE account_id = $1")
        .bind(account_id)
        .execute(&ctx.db)
        .await?;
    if let Some(ap) = ap {
        actors::invalidate_handlers(&ap.handlers, Some(&username));
    }
    // Users row: tokens/sessions/2FA/webauthn/backup codes cascade;
    // the email address is freed for reuse.
    sqlx::query("DELETE FROM users WHERE account_id = $1")
        .bind(account_id)
        .execute(&ctx.db)
        .await?;

    // Content and relationships — every DELETE is idempotent.
    let statements = [
        "DELETE FROM statuses WHERE account_id = $1",
        "DELETE FROM favourites WHERE account_id = $1",
        "DELETE FROM bookmarks WHERE account_id = $1",
        "DELETE FROM emoji_reactions WHERE account_id = $1",
        "DELETE FROM follows WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM follow_requests WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM blocks WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM mutes WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM account_domain_blocks WHERE account_id = $1",
        "DELETE FROM lists WHERE account_id = $1",
        "DELETE FROM account_notes WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM subscriptions WHERE account_id = $1 OR target_account_id = $1",
        "DELETE FROM notifications WHERE account_id = $1 OR from_account_id = $1",
        "DELETE FROM filters WHERE account_id = $1",
        "DELETE FROM invites WHERE created_by_account_id = $1",
        "DELETE FROM web_push_subscriptions WHERE account_id = $1",
        "DELETE FROM media_attachments WHERE account_id = $1",
        "DELETE FROM archive_exports WHERE account_id = $1",
        "DELETE FROM tag_follows WHERE account_id = $1",
        "DELETE FROM reports WHERE account_id = $1",
        "DELETE FROM status_pins WHERE account_id = $1",
        "DELETE FROM poll_votes WHERE account_id = $1",
        // The account's own activities except the Delete — its
        // `deliveries` rows cascade on delete and must survive to
        // finish the fan-out; the retention prune collects it later.
        "DELETE FROM activities WHERE actor_id IN (SELECT actor_id FROM accounts WHERE id = $1) \
         AND activity_id NOT LIKE '%#delete'",
    ];
    for stmt in statements {
        sqlx::query(stmt).bind(account_id).execute(&ctx.db).await?;
    }

    // Media bytes — the store routes to the configured backend.
    if let Err(e) = crate::media::storage::delete_account_files(&ctx.media, account_id).await {
        tracing::warn!(account_id, error = %e, "account media cleanup failed");
    }

    sqlx::query(
        "UPDATE account_deletions SET state = 'completed', updated_at = now() \
         WHERE account_id = $1",
    )
    .bind(account_id)
    .execute(&ctx.db)
    .await?;
    tracing::info!(account_id, username, "account deleted");
    Ok(())
}

/// Register this module's job handler.
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(
        JobKind::AccountDeletion,
        AccountDeletionJob::new(ap.cloned()),
    );
}
