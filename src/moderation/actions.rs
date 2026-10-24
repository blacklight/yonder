//! User-level moderation actions (epic 21) — the effects behind
//! `POST /api/v1/admin/accounts/:id/action` and the reversal
//! endpoints. Every application writes exactly one
//! `moderation_actions` row and one `audit_log` row, committed in the
//! same transaction as the flag change, and resolves the linked
//! report when `report_id` is given.
//!
//! Action vocabulary is Mastodon's (`none`/`sensitive`/`disable`/
//! `silence`/`suspend`) plus Yonder's `delete`; `limit` is accepted
//! as a `silence` alias. Reversals are Mastodon's dedicated verbs
//! (`unsilence`/`unsuspend`/`unsensitive`/`enable`), not `none`.
//!
//! Authorization guards beyond the scope check: moderators can't act
//! on staff (admin/moderator targets are admin-only), and `suspend`,
//! `disable` and `delete` refuse the last *active* local admin —
//! locking the instance out of its own moderation surface.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::http::AppState;
use crate::{Error, Result};

use super::audit;

/// Mastodon `type` values the action endpoint accepts (plus `limit`
/// as a `silence` alias and `delete`, admin-only).
pub fn normalize_action_type(ty: &str) -> Option<&'static str> {
    match ty {
        "none" => Some("none"),
        "sensitive" => Some("sensitive"),
        "disable" => Some("disable"),
        "silence" | "limit" => Some("silence"),
        "suspend" => Some("suspend"),
        "delete" => Some("delete"),
        _ => None,
    }
}

/// Apply `ty` to `target_id` on behalf of `actor_id` (role
/// `actor_role`).
///
/// Effects: `silence` → `limited_at`; `suspend` → `suspended_at` +
/// access revocation; `disable` → `disabled_at` + access revocation;
/// `sensitive` → `force_sensitive`; `none` → warning only;
/// `delete` → the shared account-deletion pipeline (immediate purge).
/// `report_id` links the action back to a report (marked resolved).
pub async fn apply(
    state: &AppState,
    actor_id: i64,
    actor_role: &str,
    target_id: i64,
    ty: &str,
    reason: &str,
    report_id: Option<i64>,
) -> Result<()> {
    let target = sqlx::query(
        "SELECT role, suspended_at, domain FROM accounts WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(target_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;
    let target_role: String = target.get("role");
    let target_suspended = target
        .get::<Option<DateTime<Utc>>, _>("suspended_at")
        .is_some();
    let target_local = target.get::<Option<String>, _>("domain").is_none();

    // Moderators can't act on staff — suspension-class powers over
    // admins and fellow moderators belong to admins alone.
    if actor_role == "moderator" && matches!(target_role.as_str(), "admin" | "moderator") {
        return Err(Error::Forbidden);
    }

    // Suspending, freezing or deleting the last active local admin
    // locks the instance out of its own moderation surface.
    if matches!(ty, "suspend" | "disable" | "delete")
        && target_local
        && target_role == "admin"
        && !target_suspended
    {
        let active_admins: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM accounts \
             WHERE role = 'admin' AND domain IS NULL AND deleted_at IS NULL \
               AND suspended_at IS NULL AND id <> $1",
        )
        .bind(target_id)
        .fetch_one(&state.db)
        .await?;
        if active_admins == 0 {
            return Err(Error::Conflict(format!(
                "cannot {ty} the last active admin"
            )));
        }
    }

    if ty == "delete" {
        // Immediate purge; `requested_by` records the admin. The
        // pipeline suspends + revokes itself and enqueues the job —
        // it owns its own transaction, so it runs before the
        // bookkeeping tx below.
        crate::users::deletion::schedule_account_deletion(
            state,
            target_id,
            Some(actor_id),
            std::time::Duration::ZERO,
        )
        .await?;
    }

    let mut tx = state.db.begin().await?;
    match ty {
        "silence" => {
            sqlx::query(
                "UPDATE accounts SET limited_at = now(), updated_at = now() \
                 WHERE id = $1 AND limited_at IS NULL",
            )
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        }
        "suspend" => set_flag(&mut tx, target_id, "suspended_at").await?,
        "disable" => set_flag(&mut tx, target_id, "disabled_at").await?,
        "sensitive" => {
            sqlx::query(
                "UPDATE accounts SET force_sensitive = true, updated_at = now() WHERE id = $1",
            )
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        }
        "delete" | "none" => {}
        _ => return Err(Error::Validation(format!("unknown action type {ty:?}"))),
    }

    // The stored type is the wire name — `limit` is normalized to
    // `silence` by the caller before this runs.
    sqlx::query(
        "INSERT INTO moderation_actions (account_id, type, reason, created_by_account_id) \
         VALUES ($1,$2,$3,$4)",
    )
    .bind(target_id)
    .bind(ty)
    .bind(reason)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;

    if let Some(report_id) = report_id {
        // Resolving through the action is best-effort: an
        // already-resolved or dangling report isn't an error here.
        match super::reports::resolve(&mut tx, report_id, actor_id).await {
            Ok(_) | Err(Error::Conflict(_)) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }

    audit::record_action(
        &mut *tx,
        actor_id,
        &format!("account.{ty}"),
        "account",
        target_id,
        reason,
        json!({ "type": ty, "report_id": report_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Reversal verbs: `unsilence`/`unsuspend`/`unsensitive`/`enable`
/// lift the corresponding flag. Access is NOT restored — revoked
/// tokens are gone for good; the account must re-login (same rule
/// Mastodon applies on unsuspend).
pub async fn reverse(state: &AppState, actor_id: i64, target_id: i64, verb: &str) -> Result<()> {
    let (set, action) = match verb {
        "unsilence" => ("limited_at = NULL", "account.unsilence"),
        "unsuspend" => ("suspended_at = NULL", "account.unsuspend"),
        "unsensitive" => ("force_sensitive = false", "account.unsensitive"),
        "enable" => ("disabled_at = NULL", "account.enable"),
        _ => return Err(Error::Validation(format!("unknown reversal {verb:?}"))),
    };
    let mut tx = state.db.begin().await?;
    sqlx::query(&format!(
        "UPDATE accounts SET {set}, updated_at = now() WHERE id = $1"
    ))
    .bind(target_id)
    .execute(&mut *tx)
    .await?;
    audit::record_action(
        &mut *tx,
        actor_id,
        action,
        "account",
        target_id,
        "",
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Suspend or freeze + revoke access inside `tx` — the flag update
/// and `users::deletion::revoke_access` commit atomically with the
/// action and audit rows, so a crash can't leave a suspended account
/// with live tokens. Idempotent: an already-set flag skips both the
/// update and the revoke (the first application cleared them).
async fn set_flag(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
    column: &str,
) -> Result<()> {
    let column = match column {
        "suspended_at" | "disabled_at" => column,
        _ => return Err(Error::Internal("bad set_flag call".into())),
    };
    let applied = sqlx::query(&format!(
        "UPDATE accounts SET {column} = now(), updated_at = now() \
         WHERE id = $1 AND {column} IS NULL"
    ))
    .bind(account_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if applied > 0 {
        crate::users::deletion::revoke_access(tx, account_id).await?;
    }
    Ok(())
}

/// Change `accounts.role` — admin-only (the route enforces), guarded
/// against demoting the last *active* admin. Demoting a suspended
/// admin is allowed: they don't count toward usable admin access.
/// Returns the new role.
pub async fn set_role(state: &AppState, actor_id: i64, target_id: i64, role: &str) -> Result<()> {
    if !matches!(role, "user" | "moderator" | "admin") {
        return Err(Error::Validation(format!("invalid role {role:?}")));
    }
    let mut tx = state.db.begin().await?;
    let target = sqlx::query(
        "SELECT role, suspended_at FROM accounts \
         WHERE id = $1 AND domain IS NULL AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(target_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(target) = target else {
        return Err(Error::NotFound("account".into()));
    };
    let current: String = target.get("role");
    let suspended = target
        .get::<Option<DateTime<Utc>>, _>("suspended_at")
        .is_some();
    if current == "admin" && role != "admin" && !suspended {
        let other_active_admins: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM accounts \
             WHERE role = 'admin' AND domain IS NULL AND deleted_at IS NULL \
               AND suspended_at IS NULL AND id <> $1",
        )
        .bind(target_id)
        .fetch_one(&mut *tx)
        .await?;
        if other_active_admins == 0 {
            return Err(Error::Conflict("cannot demote the last admin".into()));
        }
    }
    sqlx::query("UPDATE accounts SET role = $2, updated_at = now() WHERE id = $1")
        .bind(target_id)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    audit::record_action(
        &mut *tx,
        actor_id,
        "account.role_change",
        "account",
        target_id,
        "",
        json!({ "from": current, "to": role }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Mastodon `Admin::Account` entity, minimal: identity + moderation
/// state flags for the panel.
pub async fn admin_account_entity(db: &PgPool, account_id: i64) -> Result<Value> {
    let row = sqlx::query(
        "SELECT id, username, domain, role, suspended_at, limited_at, disabled_at, \
                force_sensitive, created_at \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;
    Ok(json!({
        "id": row.get::<i64, _>("id").to_string(),
        "username": row.get::<String, _>("username"),
        "domain": row.get::<Option<String>, _>("domain"),
        "role": row.get::<String, _>("role"),
        "suspended": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("suspended_at").is_some(),
        "limited": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("limited_at").is_some(),
        "disabled": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("disabled_at").is_some(),
        "force_sensitive": row.get::<bool, _>("force_sensitive"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
    }))
}
