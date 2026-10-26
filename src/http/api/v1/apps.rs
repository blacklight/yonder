//! `/api/v1/apps` — OAuth application registration + verification.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{oauth, tokens},
    http::{
        middleware::auth::{bearer_token, AuthContext},
        AppState,
    },
    Error, Result,
};

/// `redirect_uris` per the Mastodon API is a single string — multiple
/// URIs are whitespace-separated, doorkeeper-style. Some clients send
/// a JSON array instead; both normalize to a list.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum RedirectUrisParam {
    One(String),
    Many(Vec<String>),
}

impl RedirectUrisParam {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => s.split_whitespace().map(str::to_string).collect(),
            Self::Many(v) => v,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateAppParams {
    pub client_name: String,
    pub redirect_uris: RedirectUrisParam,
    pub scopes: Option<String>,
    pub website: Option<String>,
    /// `false` marks a public client (PKCE-required, no secret use).
    pub confidential: Option<bool>,
}

/// `POST /api/v1/apps` — register an OAuth application; the client
/// secret is returned exactly once.
#[utoipa::path(
    post,
    path = "/api/v1/apps",
    tag = "apps",
    request_body = CreateAppParams,
    responses(
        (status = 200, description = "The created application (client_secret shown once)"),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create_app(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    auth: crate::http::middleware::auth::MaybeAuth,
    Json(params): Json<CreateAppParams>,
) -> Result<Json<Value>> {
    let (app, secret) = oauth::create_app(
        &state,
        &params.client_name,
        &params.redirect_uris.into_vec(),
        params.scopes.as_deref().unwrap_or("read"),
        params.website.as_deref(),
        params.confidential.unwrap_or(true),
        auth.0.map(|a| a.account_id),
    )
    .await?;
    let mut entity = app.to_entity();
    entity["client_secret"] = json!(secret);
    let _ = (headers, uri);
    Ok(Json(entity))
}

/// `GET /api/v1/apps/verify_credentials` — the app token's application.
#[utoipa::path(
    get,
    path = "/api/v1/apps/verify_credentials",
    tag = "apps",
    responses(
        (status = 200, description = "The application behind the app token"),
        (status = 401, description = "Missing or invalid app token", body = crate::error::ErrorBody),
    )
)]
pub async fn verify_credentials(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Value>> {
    let raw = bearer_token(&headers, &uri).ok_or(Error::Unauthorized)?;
    // Unknown/revoked → 401; infra errors propagate as 5xx.
    let app = tokens::resolve_app_token(&state, &raw)
        .await?
        .ok_or(Error::Unauthorized)?;
    Ok(Json(json!({
        "id": app.application_id.to_string(),
        "name": app.name,
        "website": app.website,
        "redirect_uri": app.redirect_uris.first().cloned().unwrap_or_default(),
        "vapid_key": Value::Null,
        "client_id": app.client_id,
    })))
}

/// `GET /api/v1/oauth/authorized_applications` — the user's grants.
#[utoipa::path(
    get,
    path = "/api/v1/oauth/authorized_applications",
    tag = "oauth",
    responses(
        (status = 200, description = "Authorized applications"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn authorized_applications(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    Ok(Json(Value::Array(
        oauth::list_authorized_applications(&state.db, auth.user_id).await?,
    )))
}

/// `DELETE /api/v1/oauth/authorized_applications/{id}` — revoke a grant
/// and every token minted under it.
#[utoipa::path(
    delete,
    path = "/api/v1/oauth/authorized_applications/{id}",
    tag = "oauth",
    params(("id" = i64, Path, description = "Authorization id")),
    responses(
        (status = 200, description = "Authorization revoked"),
        (status = 404, description = "Authorization not found", body = crate::error::ErrorBody),
    )
)]
pub async fn revoke_authorization(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Value>> {
    if !oauth::revoke_authorization(&state.db, auth.user_id, id).await? {
        return Err(Error::NotFound("authorization".into()));
    }
    Ok(Json(json!({})))
}
