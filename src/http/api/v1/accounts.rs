//! `/api/v1/accounts` — registration (Mastodon `POST /api/v1/accounts`),
//! `verify_credentials`, and the `verify_email` token endpoint.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{registration, scopes::require_scope, tokens},
    http::{
        middleware::auth::{bearer_token, AuthContext},
        AppState,
    },
    users::{account as account_mapper, profile, settings},
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
    headers: HeaderMap,
) -> Result<Json<Value>> {
    credential_account(
        &state,
        &auth,
        settings::accept_language(&headers).as_deref(),
    )
    .await
}

/// The CredentialAccount entity: `to_account` plus the own-account
/// `source` block and the `yonder.settings` extension — shared by
/// `verify_credentials` and `update_credentials`.
///
/// `source.language`/`source.privacy`/`source.sensitive` are the
/// Mastodon *posting* defaults: `language` resolves stored posting
/// language → stored interface language → `Accept-Language` →
/// `i18n.fallback_locale` (see [`settings::effective_language`]).
async fn credential_account(
    state: &Arc<AppState>,
    auth: &AuthContext,
    accept_language: Option<&str>,
) -> Result<Json<Value>> {
    let account = account_mapper::load(&state.db, auth.account_id)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))?;
    let user_settings = settings::load(&state.db, auth.user_id).await?;
    let base_url = crate::activitypub::federation_base_url(&state.config);
    let mut entity = account_mapper::to_account(&account, &base_url);
    entity["source"] = json!({
        "privacy": settings::public_visibility(&user_settings.default_visibility),
        "sensitive": user_settings.default_sensitive,
        "language": settings::effective_language(
            user_settings.default_language.as_deref(),
            user_settings.locale.as_deref(),
            accept_language,
            &state.config,
        ),
        "note": account.bio.clone().unwrap_or_default(),
        "fields": account_mapper::source_fields(&account.profile_fields),
    });
    // Mastodon positions these on the CredentialAccount top level,
    // not inside `source`.
    entity["hide_collections"] = json!(user_settings.hide_network);
    entity["indexable"] = json!(user_settings.indexable);
    entity["yonder"]["settings"] = user_settings.yonder_json(&state.config, accept_language);
    Ok(Json(entity))
}

/// `PATCH /api/v1/accounts/update_credentials` — edit the caller's
/// profile (display name, bio, avatar/header, ordered fields, and the
/// `locked`/`bot`/`discoverable` flags). Accepts
/// `multipart/form-data` (file uploads), `application/x-www-form-urlencoded`
/// and `application/json` (no files). Returns the CredentialAccount.
#[utoipa::path(
    patch,
    path = "/api/v1/accounts/update_credentials",
    tag = "accounts",
    request_body(content_type = "application/json", content = crate::users::profile::UpdateCredentialsSchema),
    responses(
        (status = 200, description = "Updated CredentialAccount"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 413, description = "Body exceeds the upload limit", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn update_credentials(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:accounts")?;
    let accept_language = settings::accept_language(request.headers());
    let params = profile::parse_request(&state, request).await?;
    profile::update_profile(&state, &auth, params).await?;
    credential_account(&state, &auth, accept_language.as_deref()).await
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
