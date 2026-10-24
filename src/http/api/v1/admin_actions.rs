//! `/api/v1/admin/accounts/{id}/action`, `.../role`, `.../unsilence`,
//! `.../unsuspend`, `.../unsensitive`, `.../enable` and
//! `GET /api/v1/admin/accounts/{id}` — the user-level moderation
//! surface (epic 21).
//!
//! Moderators can warn/silence/sensitive/disable/suspend; `delete`
//! and role changes are admin-only. Every effect lives in
//! [`crate::moderation::actions`] — handlers only validate, authorize
//! and audit.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::{require_moderator_scope, require_scope},
    http::{middleware::auth::AuthContext, AppState},
    moderation::actions,
    Error, Result,
};

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
    require_moderator_scope(&state.config, &auth, "admin:write")?;
    let ty = actions::normalize_action_type(&body.ty)
        .ok_or_else(|| Error::Validation(format!("unknown action type {:?}", body.ty)))?;
    if ty == "delete" && auth.role != "admin" {
        return Err(Error::Forbidden);
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
    require_moderator_scope(&state.config, &auth, "admin:write")?;
    if !matches!(
        verb.as_str(),
        "unsilence" | "unsuspend" | "unsensitive" | "enable"
    ) {
        return Err(Error::NotFound("route".into()));
    }
    actions::reverse(&state, auth.account_id, id, &verb).await?;
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
    require_scope(&state.config, &auth, "admin")?;
    actions::set_role(&state, auth.account_id, id, &body.role).await?;
    Ok(Json(actions::admin_account_entity(&state.db, id).await?))
}

#[derive(Deserialize)]
pub struct RoleBody {
    pub role: String,
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
    require_moderator_scope(&state.config, &auth, "admin:read")?;
    let entity = actions::admin_account_entity(&state.db, id).await?;
    Ok(Json(json!(entity)))
}
