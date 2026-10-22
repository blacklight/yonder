//! `/api/v1/tokens` — user-managed API tokens (Yonder extension).

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::tokens,
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateTokenParams {
    pub name: String,
    pub scopes: String,
    /// Optional TTL in seconds from now.
    pub expires_in: Option<u64>,
}

/// `POST /api/v1/tokens` — create a named token scoped to a subset of
/// the caller's scopes; the plaintext is returned once.
#[utoipa::path(
    post,
    path = "/api/v1/tokens",
    tag = "tokens",
    request_body = CreateTokenParams,
    responses(
        (status = 200, description = "The created token (plaintext shown once)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 409, description = "Name already in use", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create_token(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<CreateTokenParams>,
) -> Result<Json<Value>> {
    let (id, raw) = tokens::create_user_token(
        &state,
        auth.user_id,
        &params.name,
        &params.scopes,
        params.expires_in,
        &auth.scopes,
    )
    .await?;
    Ok(Json(json!({
        "id": id.to_string(),
        "name": params.name,
        "token": raw,
    })))
}

/// `GET /api/v1/tokens` — the caller's active named tokens.
#[utoipa::path(
    get,
    path = "/api/v1/tokens",
    tag = "tokens",
    responses(
        (status = 200, description = "Active tokens"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    Ok(Json(Value::Array(
        tokens::list_user_tokens(&state.db, auth.user_id).await?,
    )))
}

/// `DELETE /api/v1/tokens/{id}` — revoke a token.
#[utoipa::path(
    delete,
    path = "/api/v1/tokens/{id}",
    tag = "tokens",
    params(("id" = i64, Path, description = "Token id")),
    responses(
        (status = 200, description = "Token revoked"),
        (status = 404, description = "Token not found", body = crate::error::ErrorBody),
    )
)]
pub async fn revoke_token(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Value>> {
    if !tokens::revoke_user_token(&state.db, auth.user_id, id).await? {
        return Err(Error::NotFound("token".into()));
    }
    Ok(Json(json!({})))
}
