//! `/api/v1/yonder/invites` (epic 11): create/list/revoke invite links
//! and the unauthenticated validity check the sign-up UI calls under
//! closed registration.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    users::invites,
    Result,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateInviteParams {
    /// Maximum number of uses (`null` = unlimited).
    pub max_uses: Option<i32>,
    /// Absolute expiry, RFC 3339 (mutually exclusive with `expires_in`).
    pub expires_at: Option<String>,
    /// Lifetime in seconds.
    pub expires_in: Option<u64>,
}

/// `POST /api/v1/yonder/invites` — create an invite link.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/invites",
    tag = "invites",
    request_body = CreateInviteParams,
    responses(
        (status = 200, description = "Created invite"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Invites disabled or insufficient role", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<CreateInviteParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    if !invites::may_create(&state.config(), &auth.role) {
        return Err(crate::Error::Forbidden);
    }
    if params.expires_at.is_some() && params.expires_in.is_some() {
        return Err(crate::Error::Validation(
            "expires_at and expires_in are mutually exclusive".into(),
        ));
    }
    let expires_at = match (params.expires_at, params.expires_in) {
        (Some(at), _) => Some(
            chrono::DateTime::parse_from_rfc3339(&at)
                .map_err(|_| crate::Error::Validation("expires_at must be RFC 3339".into()))?
                .with_timezone(&chrono::Utc),
        ),
        (None, Some(secs)) => {
            Some(chrono::Utc::now() + chrono::Duration::seconds(secs.min(31_536_000) as i64))
        }
        _ => None,
    };
    let invite = invites::create(&state, auth.account_id, params.max_uses, expires_at).await?;
    Ok(Json(invite))
}

/// `GET /api/v1/yonder/invites` — list the caller's invites.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/invites",
    tag = "invites",
    responses(
        (status = 200, description = "The caller's invites"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list_invites(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    Ok(Json(Value::Array(
        invites::list(&state, auth.account_id).await?,
    )))
}

/// `DELETE /api/v1/yonder/invites/{id}` — revoke an invite.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/invites/{id}",
    tag = "invites",
    params(("id" = i64, Path, description = "Invite id")),
    responses(
        (status = 200, description = "Invite revoked"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown invite", body = crate::error::ErrorBody),
    )
)]
pub async fn revoke_invite(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let staff = matches!(auth.role.as_str(), "admin" | "moderator");
    invites::revoke(&state, auth.account_id, staff, id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `GET /api/v1/yonder/invites/check/{code}` — public validity check.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/invites/check/{code}",
    tag = "invites",
    params(("code" = String, Path, description = "Invite code")),
    responses((status = 200, description = "{ \"valid\": bool }"))
)]
pub async fn check_invite(
    State(state): State<Arc<AppState>>,
    Path(code): Path<String>,
) -> Result<Json<Value>> {
    invites::check(&state, &code).await.map(Json)
}
