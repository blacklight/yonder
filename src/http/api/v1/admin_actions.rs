//! `/api/v1/admin/accounts/{id}/action`, `.../role`, `.../unsilence`,
//! `.../unsuspend`, `.../unsensitive`, `.../enable` and
//! `GET /api/v1/admin/accounts/{id}` — the user-level moderation
//! surface (epic 21).
//!
//! Moderators can warn/silence/sensitive/disable/suspend; `delete`
//! and role changes are admin-only. Every effect lives in
//! [`crate::moderation::actions`] — handlers only validate, authorize
//! and audit.

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{
        password::{hash_password, MIN_PASSWORD_LENGTH},
        registration,
        scopes::{require_moderator_scope, require_scope},
    },
    http::{middleware::auth::AuthContext, paginate, AppState},
    interactions::boost,
    moderation::{actions, audit},
    posts::{self, PostContext},
    Error, Result,
};

/// `GET /api/v1/admin/accounts` — Mastodon's admin account index:
/// `origin` (`local`|`remote`), `status` (`active`|`pending`|
/// `disabled`|`silenced`|`suspended`) and the individual flags
/// (`local`, `remote`, `staff`, `suspended`, `disabled`, `silenced`,
/// `pending`, `active`) combine conjunctively; `username`,
/// `display_name`, `email` are substring matches, `by_domain` exact,
/// `role` (Yonder extension) exact. `max_id`/`min_id`/`limit`
/// keyset-paginate; `Link` headers carry cursors. Moderator+.
#[utoipa::path(
    get,
    path = "/api/v1/admin/accounts",
    tag = "admin",
    params(
        ("origin" = Option<String>, Query, description = "`local` or `remote`"),
        ("status" = Option<String>, Query, description = "`active|pending|disabled|silenced|suspended`"),
        ("username" = Option<String>, Query, description = "Substring match"),
        ("display_name" = Option<String>, Query, description = "Substring match"),
        ("email" = Option<String>, Query, description = "Substring match"),
        ("by_domain" = Option<String>, Query, description = "Exact remote domain"),
        ("role" = Option<String>, Query, description = "`user|moderator|admin`"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)"),
    ),
    responses(
        (status = 200, description = "Array of Admin::Account, newest first"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid filter value", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_moderator_scope(&state.config(), &auth, "admin:read")?;
    let page = paginate::parse(&params);
    let filters = parse_filters(&params)?;
    let rows = actions::list(&state.db, &filters, &page).await?;
    let cursors: Vec<i64> = rows
        .iter()
        .filter_map(|row| row["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(rows))))
}

/// Query-string → [`actions::AccountFilters`]. Unknown `origin`,
/// `status` or `role` values are 422s — silently dropping a filter
/// would widen the result set.
fn parse_filters(params: &HashMap<String, String>) -> Result<actions::AccountFilters> {
    let truthy = |k: &str| {
        params
            .get(k)
            .is_some_and(|v| matches!(v.as_str(), "true" | "1"))
    };
    let text = |k: &str| {
        params
            .get(k)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let mut f = actions::AccountFilters {
        local: truthy("local"),
        remote: truthy("remote"),
        staff: truthy("staff"),
        suspended: truthy("suspended"),
        disabled: truthy("disabled"),
        silenced: truthy("silenced") || truthy("limited"),
        pending: truthy("pending"),
        active: truthy("active"),
        role: text("role"),
        username: text("username"),
        display_name: text("display_name"),
        email: text("email"),
        by_domain: text("by_domain"),
        search: text("search"),
    };
    match params.get("origin").map(|v| v.trim()) {
        Some("local") => f.local = true,
        Some("remote") => f.remote = true,
        Some(other) => {
            return Err(Error::Validation(format!("invalid origin {other:?}")));
        }
        None => {}
    }
    match params.get("status").map(|v| v.trim()) {
        Some("suspended") => f.suspended = true,
        Some("disabled") => f.disabled = true,
        Some("silenced" | "limited") => f.silenced = true,
        Some("pending") => f.pending = true,
        Some("active") => f.active = true,
        Some(other) => {
            return Err(Error::Validation(format!("invalid status {other:?}")));
        }
        None => {}
    }
    if let Some(role) = &f.role {
        if !matches!(role.as_str(), "user" | "moderator" | "admin") {
            return Err(Error::Validation(format!("invalid role {role:?}")));
        }
    }
    Ok(f)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateAccountBody {
    pub username: String,
    pub email: String,
    pub password: String,
    /// `user|moderator|admin` (default `user`).
    #[serde(default)]
    pub role: Option<String>,
    /// Create the account already verified (implied for staff roles).
    #[serde(default)]
    pub skip_verification: bool,
    #[serde(default)]
    pub locale: Option<String>,
}

/// `POST /api/v1/admin/accounts` — Yonder extension (Mastodon has no
/// admin create; Pleroma's `/pleroma/admin/users` is the analog):
/// create a local account bypassing the registration mode. Shares
/// [`registration::create_admin_account`] with `yonder user create`
/// — approved, role set, verification email when required and not
/// skipped. Admin-only; the caller is the audit actor.
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "The new Admin::Account"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failed", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<CreateAccountBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    if body.password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(Error::Validation(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    let password_hash = hash_password(&body.password)?;
    let registered = registration::create_admin_account(
        &state,
        &registration::AdminCreateParams {
            username: &body.username,
            email: &body.email,
            password_hash: &password_hash,
            role: body.role.as_deref().unwrap_or("user"),
            skip_verification: body.skip_verification,
            locale: body.locale.as_deref(),
            actor_id: Some(auth.account_id),
        },
    )
    .await?;
    Ok(Json(
        actions::admin_account_entity(&state.db, registered.account_id).await?,
    ))
}

/// `DELETE /api/v1/admin/accounts/{id}` — REST alias for
/// `POST …/action` with `type: "delete"`: same admin-only guard,
/// same last-active-admin protection, same audit row, same
/// immediate-purge pipeline. Remote accounts are permitted (their
/// local mirror is removed; Mastodon admins purge remote accounts the
/// same way).
#[utoipa::path(
    delete,
    path = "/api/v1/admin/accounts/{id}",
    tag = "admin",
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "Deletion scheduled"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
        (status = 409, description = "Last active admin", body = crate::error::ErrorBody),
        (status = 422, description = "Self-target", body = crate::error::ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    if id == auth.account_id {
        return Err(Error::Validation("cannot delete your own account".into()));
    }
    actions::apply(&state, auth.account_id, &auth.role, id, "delete", "", None).await?;
    Ok(Json(json!({ "id": id.to_string(), "deleted": true })))
}

/// `POST /api/v1/admin/accounts/{id}/action` — Mastodon form:
/// `type` (required; `none|sensitive|disable|silence|suspend` plus
/// Yonder's `delete`, admin-only, and `limit` as a `silence` alias),
/// `report_id`, `text` (reason, ≤ 2000 chars),
/// `send_email_notification` (accepted; the account-status email is
/// epic 22's).
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts/{id}/action",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "Action applied"),
        (status = 403, description = "Moderator/admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid action type", body = crate::error::ErrorBody),
    )
)]
pub async fn action(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<ActionBody>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    let ty = actions::normalize_action_type(&body.ty)
        .ok_or_else(|| Error::Validation(format!("unknown action type {:?}", body.ty)))?;
    if ty == "delete" && auth.role != "admin" {
        return Err(Error::Forbidden("account deletion requires the admin role"));
    }
    if id == auth.account_id {
        return Err(Error::Validation(
            "cannot take moderation actions on your own account".into(),
        ));
    }
    let reason = body.text.unwrap_or_default();
    if reason.chars().count() > 2000 {
        return Err(Error::Validation("reason exceeds 2000 characters".into()));
    }
    actions::apply(
        &state,
        auth.account_id,
        &auth.role,
        id,
        ty,
        &reason,
        body.report_id,
    )
    .await?;
    // The account-status mail honors `send_email_notification`; the
    // `email.send_account_status` class flag gates at the job.
    if body.send_email_notification.unwrap_or(false) {
        if let Err(e) = state
            .dispatcher
            .enqueue(
                crate::jobs::JobKind::EmailSend,
                json!({
                    "kind": "account_status",
                    "account_id": id,
                    "status": ty,
                    "reason": reason,
                }),
            )
            .await
        {
            tracing::warn!(error = %e, account_id = id, "account-status email enqueue failed");
        }
    }
    Ok(Json(actions::admin_account_entity(&state.db, id).await?))
}

#[derive(Deserialize)]
pub struct ActionBody {
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub report_id: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub send_email_notification: Option<bool>,
}

#[derive(Deserialize)]
pub struct StatusActionBody {
    #[serde(rename = "type")]
    pub ty: String,
}

/// `POST /api/v1/admin/statuses/{id}/action` — Yonder extension
/// (epic 24): status-level moderation. `type` is `sensitive`
/// (flag the status so its media/content render behind a warning)
/// or `delete` (tombstone it; boost rows unreblog instead, matching
/// the owner-delete path). Moderators and admins; each applied
/// action writes an audit row.
#[utoipa::path(
    post,
    path = "/api/v1/admin/statuses/{id}/action",
    tag = "admin",
    request_body(content_type = "application/json"),
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "Action applied"),
        (status = 403, description = "Moderator/admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such status", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid action type", body = crate::error::ErrorBody),
    )
)]
pub async fn status_action(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<StatusActionBody>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    let audit_action = match body.ty.as_str() {
        "sensitive" => "status.mark_sensitive",
        "delete" => "status.delete",
        _ => {
            return Err(Error::Validation(format!(
                "unknown status action type {:?}",
                body.ty
            )))
        }
    };
    let status = posts::load(&state.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    match body.ty.as_str() {
        "sensitive" => {
            // Flag + audit atomically, then propagate: live clients get
            // the re-rendered entity, the AP object cache is dropped,
            // and local statuses federate an `Update` (same pattern as
            // `tombstone`'s `Delete`).
            let mut tx = state.db.begin().await?;
            sqlx::query("UPDATE statuses SET sensitive = true, updated_at = now() WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            audit::record(
                &mut *tx,
                Some(auth.account_id),
                audit_action,
                "status",
                Some(id),
                json!({ "type": body.ty }),
            )
            .await?;
            tx.commit().await?;
            let ctx = PostContext::new(&state);
            crate::timelines::stream::publish_status_update(&ctx, &status).await;
            if let Some(ap) = &ctx.ap {
                if status.local {
                    if let Err(e) =
                        crate::activitypub::posts::publish_update(ap, &ctx, &status, &[]).await
                    {
                        tracing::error!(status_id = id, error = %e, "AP Update publish failed");
                    }
                }
                crate::activitypub::posts::invalidate_object(ap, id);
            }
        }
        "delete" => {
            let ctx = PostContext::new(&state);
            if status.reblog_of_id.is_some() {
                boost::unreblog_row(&ctx, &status).await?;
            } else {
                posts::create::tombstone(&ctx, &status).await?;
            }
            audit::record(
                &state.db,
                Some(auth.account_id),
                audit_action,
                "status",
                Some(id),
                json!({ "type": body.ty }),
            )
            .await?;
        }
        _ => unreachable!(),
    }
    Ok(Json(json!({
        "id": id.to_string(),
        "action": body.ty,
        "applied": true,
    })))
}

/// `POST /api/v1/admin/accounts/{id}/{unsilence,unsuspend,unsensitive,enable}`
/// — Mastodon's reversal verbs. Moderators and admins.
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts/{id}/unsuspend",
    tag = "admin",
    responses(
        (status = 200, description = "Reversal applied"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
    )
)]
pub async fn reverse(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path((id, verb)): Path<(i64, String)>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    if !matches!(
        verb.as_str(),
        "unsilence" | "unsuspend" | "unsensitive" | "enable"
    ) {
        return Err(Error::NotFound("route".into()));
    }
    actions::reverse(&state, auth.account_id, &auth.role, id, &verb).await?;
    Ok(Json(actions::admin_account_entity(&state.db, id).await?))
}

/// `POST /api/v1/admin/accounts/{id}/role` — Yonder extension
/// (Mastodon manages roles in the panel, not over the API). Admin
/// only; the last admin can't be demoted.
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts/{id}/role",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "Role changed"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
        (status = 409, description = "Last admin", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid role", body = crate::error::ErrorBody),
    )
)]
pub async fn set_role(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<RoleBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    actions::set_role(&state, auth.account_id, id, &body.role).await?;
    Ok(Json(actions::admin_account_entity(&state.db, id).await?))
}

#[derive(Deserialize)]
pub struct RoleBody {
    pub role: String,
}

/// `POST /api/v1/admin/accounts/{id}/reset_password` — Yonder
/// extension (epic 22): generate a one-time reset link, returned in
/// the response *and* mailed to the user when the
/// `email.send_password_reset` class flag is on. The token is a
/// Redis one-time secret (`password-reset` domain, 2 h TTL).
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts/{id}/reset_password",
    tag = "admin",
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "`reset_url` for the admin"),
        (status = 403, description = "Moderator targeting an admin or moderator", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
        (status = 422, description = "Remote, suspended or deleted account", body = crate::error::ErrorBody),
    )
)]
pub async fn reset_password(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    if id == auth.account_id {
        return Err(Error::Validation(
            "cannot take moderation actions on your own account".into(),
        ));
    }
    let url = actions::reset_link(&state, auth.account_id, &auth.role, id, "when_enabled").await?;
    Ok(Json(json!({ "reset_url": url })))
}

/// `POST /api/v1/admin/accounts/{id}/email_reset` — like
/// `reset_password` but the link is *always* emailed (the admin's
/// explicit intent bypasses the class flag) and the response only
/// confirms the send.
#[utoipa::path(
    post,
    path = "/api/v1/admin/accounts/{id}/email_reset",
    tag = "admin",
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "`sent: true`"),
        (status = 403, description = "Moderator targeting an admin or moderator", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
        (status = 422, description = "Remote, suspended or deleted account", body = crate::error::ErrorBody),
    )
)]
pub async fn email_reset(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    if id == auth.account_id {
        return Err(Error::Validation(
            "cannot take moderation actions on your own account".into(),
        ));
    }
    actions::reset_link(&state, auth.account_id, &auth.role, id, "always").await?;
    Ok(Json(json!({ "sent": true })))
}

/// `GET /api/v1/admin/accounts/{id}` — the moderation-state view.
/// (Epic 22 extends this into the full `Admin::Account`.)
#[utoipa::path(
    get,
    path = "/api/v1/admin/accounts/{id}",
    tag = "admin",
    responses(
        (status = 200, description = "Account + moderation flags"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such account", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:read")?;
    let entity = actions::admin_account_entity(&state.db, id).await?;
    Ok(Json(json!(entity)))
}
