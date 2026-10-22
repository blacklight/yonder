//! `/api/v1/accounts` — registration (Mastodon `POST /api/v1/accounts`),
//! `verify_credentials`, and the `verify_email` token endpoint.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{registration, tokens},
    http::{
        middleware::auth::{bearer_token, AuthContext},
        AppState,
    },
    Error, Result,
};

/// `POST /api/v1/accounts` — register. An optional bearer **app token**
/// (client_credentials) binds the returned user token to that app;
/// anonymous registration is allowed.
#[utoipa::path(
    post,
    path = "/api/v1/accounts",
    tag = "accounts",
    request_body = registration::RegisterParams,
    responses(
        (status = 200, description = "Account created; returns an access token"),
        (status = 401, description = "Invalid app token", body = crate::error::ErrorBody),
        (status = 403, description = "Registrations closed", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Json(params): Json<registration::RegisterParams>,
) -> Result<Json<Value>> {
    // App token (client_credentials, user_id NULL) — a dedicated lookup,
    // not the user-only AuthContext. A presented-but-invalid token 401s;
    // infrastructure errors propagate as 5xx (a DB outage isn't a bad
    // token).
    let app = match bearer_token(&headers, &uri) {
        Some(raw) => Some(
            tokens::resolve_app_token(&state, &raw)
                .await?
                .ok_or(Error::Unauthorized)?,
        ),
        None => None,
    };

    // Account + actor + first token pair commit in one transaction.
    let (_registered, issued) = registration::register(&state, &params, app.as_ref()).await?;
    Ok(Json(tokens::token_entity(&issued)))
}

/// `GET /api/v1/accounts/verify_credentials` — the caller's Account entity.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/verify_credentials",
    tag = "accounts",
    responses(
        (status = 200, description = "The authenticated account"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn verify_credentials(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    let account = crate::users::account::load(&state.db, auth.account_id)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))?;
    let mut entity = crate::users::account::to_account(&account);
    let locale: Option<String> = sqlx::query_scalar("SELECT locale FROM users WHERE id = $1")
        .bind(auth.user_id)
        .fetch_one(&state.db)
        .await?;
    entity["source"] = json!({
        "privacy": Value::Null,
        "sensitive": false,
        "language": locale,
        "note": account.bio.clone().unwrap_or_default(),
        "fields": account.profile_fields,
    });
    Ok(Json(entity))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct VerifyEmailParams {
    pub token: String,
}

/// `POST /api/v1/accounts/verify_email` — consume an emailed token.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/verify_email",
    tag = "accounts",
    request_body = VerifyEmailParams,
    responses(
        (status = 200, description = "Email verified"),
        (status = 422, description = "Invalid or expired token", body = crate::error::ErrorBody),
    )
)]
pub async fn verify_email(
    State(state): State<Arc<AppState>>,
    Json(params): Json<VerifyEmailParams>,
) -> Result<Json<Value>> {
    if !registration::verify_email(&state, &params.token).await? {
        return Err(Error::Validation("invalid or expired token".into()));
    }
    Ok(Json(json!({ "verified": true })))
}
