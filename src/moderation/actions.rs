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
/// as a `silence` alias and `delete`, admin-only), and epic 22's
/// account-hygiene actions — the `moderation_actions.type` CHECK has
/// allowed them since migration `0006`.
pub fn normalize_action_type(ty: &str) -> Option<&'static str> {
    match ty {
        "none" => Some("none"),
        "sensitive" => Some("sensitive"),
        "disable" => Some("disable"),
        "silence" | "limit" => Some("silence"),
        "suspend" => Some("suspend"),
        "delete" => Some("delete"),
        "disable_2fa" => Some("disable_2fa"),
        "require_password_reset" => Some("require_password_reset"),
        "verify_email" => Some("verify_email"),
        _ => None,
    }
}

/// Transaction-scoped advisory lock key serializing every mutation
/// that evaluates the set of active local admins (`apply` for
/// suspend/disable/delete of an admin, `set_role` demotions). All
/// guard reads — the target's role/suspension and the active-admin
/// count — run *after* taking this lock, so two concurrent requests
/// can't both pass a check that assumes the other hasn't landed.
/// Arbitrary constant; only has to agree across callers.
const ADMIN_SET_LOCK: i64 = 0x596F_6E64_6572; // "Yonder"

async fn admin_set_lock(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(ADMIN_SET_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Resolve a local target's `users.id` — the account-hygiene actions
/// (epic 22) only apply to local accounts.
async fn local_user_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
) -> Result<i64> {
    sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::Validation("action applies to local accounts only".into()))
}

/// Apply `ty` to `target_id` on behalf of `actor_id` (role
/// `actor_role`).
///
/// Effects: `silence` → `limited_at`; `suspend` → `suspended_at` +
/// access revocation; `disable` → `disabled_at` + access revocation;
/// `sensitive` → `force_sensitive`; `none` → warning only;
/// `delete` → the shared account-deletion pipeline (immediate purge).
/// `report_id` links the action back to a report (marked resolved).
///
/// The guard reads (target role/suspension, active-admin count) run
/// inside the transaction under [`admin_set_lock`]; the flag change,
/// `moderation_actions` row, linked-report resolution and audit row
/// all commit atomically with them.
pub async fn apply(
    state: &AppState,
    actor_id: i64,
    actor_role: &str,
    target_id: i64,
    ty: &str,
    reason: &str,
    report_id: Option<i64>,
) -> Result<()> {
    let mut tx = state.db.begin().await?;
    admin_set_lock(&mut tx).await?;
    let target = sqlx::query(
        "SELECT role, suspended_at, domain FROM accounts WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(target_id)
    .fetch_optional(&mut *tx)
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
        .fetch_one(&mut *tx)
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
        // it owns its own transaction, so a failure of our
        // bookkeeping below can't undo it (the action/audit rows
        // would be missing). It runs here — inside the advisory-lock
        // window, after the guards — but before our tx takes any row
        // lock on the target, so the two transactions can't deadlock.
        crate::users::deletion::schedule_account_deletion(
            state,
            target_id,
            Some(actor_id),
            std::time::Duration::ZERO,
        )
        .await?;
    }

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
        // Epic 22's account-hygiene actions all apply to `users` rows —
        // remote accounts have none.
        "disable_2fa" => {
            let user_id = local_user_id(&mut tx, target_id).await?;
            // Owns its transaction (same precedent as `delete` above —
            // runs before this tx takes a row lock on the target).
            crate::auth::two_factor::admin_disable_2fa(&state.db, user_id).await?;
        }
        "verify_email" => {
            local_user_id(&mut tx, target_id).await?;
            sqlx::query(
                "UPDATE users SET email_verified_at = now(), updated_at = now() \
                 WHERE account_id = $1 AND email_verified_at IS NULL",
            )
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        }
        "require_password_reset" => {
            local_user_id(&mut tx, target_id).await?;
            sqlx::query(
                "UPDATE users SET require_password_reset = true, updated_at = now() \
                 WHERE account_id = $1",
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
/// Mastodon applies on unsuspend). The staff-target guard matches
/// `apply`: moderators can't undo an admin's action on staff.
pub async fn reverse(
    state: &AppState,
    actor_id: i64,
    actor_role: &str,
    target_id: i64,
    verb: &str,
) -> Result<()> {
    let (set, action) = match verb {
        "unsilence" => ("limited_at = NULL", "account.unsilence"),
        "unsuspend" => ("suspended_at = NULL", "account.unsuspend"),
        "unsensitive" => ("force_sensitive = false", "account.unsensitive"),
        "enable" => ("disabled_at = NULL", "account.enable"),
        _ => return Err(Error::Validation(format!("unknown reversal {verb:?}"))),
    };
    let mut tx = state.db.begin().await?;
    let target_role: Option<String> =
        sqlx::query_scalar("SELECT role FROM accounts WHERE id = $1 AND deleted_at IS NULL")
            .bind(target_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(target_role) = target_role else {
        return Err(Error::NotFound("account".into()));
    };
    if actor_role == "moderator" && matches!(target_role.as_str(), "admin" | "moderator") {
        return Err(Error::Forbidden);
    }
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
    // Same lock `apply` takes: role changes and staff-affecting
    // actions serialize so concurrent requests can't both pass a
    // last-admin check.
    admin_set_lock(&mut tx).await?;
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

/// One-time password-reset link lifetime (epic 22 admin actions).
const PASSWORD_RESET_TTL_SECONDS: u64 = 2 * 60 * 60;

/// Generate a one-time password-reset link for a local account and
/// audit the generation. `email` controls the `password_reset` mail:
/// `"when_enabled"` honors `email.send_password_reset` (the
/// `EmailSend` job gate does the check), `"always"` passes `force` so
/// the admin's explicit intent wins. The caller decides what the
/// response shape is — the URL is returned either way.
///
/// The token lives in Redis under the `password-reset` domain, keyed
/// by its sha256 like every other secret flow (`auth::tokens`); it is
/// redacted from the audit row. A reset link is a credential, so the
/// same staff guard as [`apply`] applies: moderators cannot mint links
/// for admins or moderators.
pub async fn reset_link(
    state: &AppState,
    actor_id: i64,
    actor_role: &str,
    target_id: i64,
    email: &str,
) -> Result<String> {
    let row: Option<(i64, String, bool)> = sqlx::query_as(
        "SELECT u.id, a.role, a.suspended_at IS NOT NULL OR a.deleted_at IS NOT NULL \
         FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.account_id = $1",
    )
    .bind(target_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((user_id, target_role, inactive)) = row else {
        return Err(Error::Validation(
            "action applies to local accounts only".into(),
        ));
    };
    if actor_role == "moderator" && matches!(target_role.as_str(), "admin" | "moderator") {
        return Err(Error::Forbidden);
    }
    if inactive {
        return Err(Error::Validation("account is suspended or deleted".into()));
    }
    let token = crate::auth::tokens::random_secret();
    crate::auth::tokens::set_secret_key(
        state,
        "password-reset",
        &token,
        &json!(user_id),
        PASSWORD_RESET_TTL_SECONDS,
    )
    .await?;
    let base = crate::activitypub::federation_base_url(&state.config());
    let url = format!("{base}/auth/password-reset/{token}");

    let emailed = match email {
        "when_enabled" => state.config().email.send_password_reset,
        "always" => true,
        _ => false,
    };
    if emailed {
        let force = email == "always";
        state
            .dispatcher
            .enqueue(
                crate::jobs::JobKind::EmailSend,
                json!({
                    "kind": "password_reset",
                    "account_id": target_id,
                    "token": token,
                    "url": url,
                    "force": force,
                }),
            )
            .await?;
    }

    let action = if email == "always" {
        "account.email_reset"
    } else {
        "account.reset_password"
    };
    audit::record(
        &state.db,
        Some(actor_id),
        action,
        "account",
        Some(target_id),
        json!({ "emailed": emailed, "token": "<redacted>" }),
    )
    .await?;
    Ok(url)
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
