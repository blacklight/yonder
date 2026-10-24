//! `/api/v1/yonder/…` account-lifecycle endpoints (epic 11): aliases,
//! the move-to redirect and its cancellation, and self-service account
//! deletion. Mastodon exposes none of these over REST — they are
//! web-UI actions — so they live under the `yonder` prefix.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    users::{deletion, moves},
    Result,
};

/// `GET /api/v1/yonder/aliases` — the caller's `alsoKnownAs` URIs.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/aliases",
    tag = "account",
    responses(
        (status = 200, description = "Alias URIs"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list_aliases(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    let aliases = moves::list_aliases(&state.db, auth.account_id).await?;
    Ok(Json(json!({ "aliases": aliases })))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AliasParams {
    /// `user@domain` handle or actor URI to add/remove.
    pub acct: String,
}

/// `POST /api/v1/yonder/aliases` — add an alias.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/aliases",
    tag = "account",
    request_body = AliasParams,
    responses(
        (status = 200, description = "Alias added"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn add_alias(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<AliasParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    moves::add_alias(&state, auth.account_id, &params.acct).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `DELETE /api/v1/yonder/aliases` — remove an alias.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/aliases",
    tag = "account",
    request_body = AliasParams,
    responses(
        (status = 200, description = "Alias removed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
    )
)]
pub async fn remove_alias(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<AliasParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    moves::remove_alias(&state, auth.account_id, &params.acct).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct MoveParams {
    /// `user@domain` handle or actor URI to move to.
    pub acct: String,
    /// Current password (confirmation for the irreversible redirect).
    pub password: String,
}

/// `POST /api/v1/yonder/move` — redirect this account to `acct`.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/move",
    tag = "account",
    request_body = MoveParams,
    responses(
        (status = 200, description = "Move scheduled; followers migrate in the background"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Wrong password or suspended", body = crate::error::ErrorBody),
        (status = 422, description = "Target is not an alias of this account, or cooldown", body = crate::error::ErrorBody),
    )
)]
pub async fn move_account(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<MoveParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    moves::move_to(&state, auth.account_id, &params.acct, &params.password).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PasswordParams {
    /// Current password.
    pub password: String,
}

/// `DELETE /api/v1/yonder/move` — cancel the redirect.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/move",
    tag = "account",
    request_body = PasswordParams,
    responses(
        (status = 200, description = "Redirect cleared"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Wrong password", body = crate::error::ErrorBody),
    )
)]
pub async fn cancel_move(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<PasswordParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    moves::cancel_move(&state, auth.account_id, &params.password).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/v1/yonder/account/delete` — schedule self-deletion.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/account/delete",
    tag = "account",
    request_body = PasswordParams,
    responses(
        (status = 200, description = "Deletion scheduled; tokens are revoked immediately"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Wrong password", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<PasswordParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let password_hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(auth.user_id)
            .fetch_one(&state.db)
            .await?;
    let Some(hash) = password_hash else {
        return Err(crate::Error::Forbidden);
    };
    if !crate::auth::password::verify_password(&params.password, &hash) {
        return Err(crate::Error::Forbidden);
    }
    let delay = std::time::Duration::from_secs(
        state.config().admin.account_deletion_grace_days as u64 * 86_400,
    );
    let id = deletion::schedule_account_deletion(&state, auth.account_id, None, delay).await?;
    Ok(Json(json!({ "deletion_id": id.to_string() })))
}
