//! `/api/v1/2fa` — two-factor management (epic 08): TOTP enrollment,
//! backup codes, WebAuthn credential lifecycle, and account-wide
//! disable. Thin handlers — logic lives in `crate::auth::two_factor`
//! and `crate::auth::webauthn`. Destructive operations re-authenticate
//! with the account password in the request body.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{scopes::require_scope, two_factor, webauthn},
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// `GET /api/v1/2fa` — the caller's 2FA posture: enabled methods,
/// per-method details and remaining backup codes.
#[utoipa::path(
    get,
    path = "/api/v1/2fa",
    tag = "two_factor",
    responses(
        (status = 200, description = "Two-factor status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
    )
)]
pub async fn status(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read")?;
    Ok(Json(two_factor::status(&state, auth.user_id).await?))
}

/// `POST /api/v1/2fa/totp/setup` — generate a pending TOTP secret.
/// The base32 secret, `otpauth://` URI and QR data URI are returned
/// exactly once; a confirmed enrollment must be removed before
/// re-enrolling.
#[utoipa::path(
    post,
    path = "/api/v1/2fa/totp/setup",
    tag = "two_factor",
    responses(
        (status = 200, description = "Pending secret + otpauth URI + QR"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 409, description = "TOTP already enabled", body = crate::error::ErrorBody),
        (status = 500, description = "auth.secret_key is not configured", body = crate::error::ErrorBody),
    )
)]
pub async fn totp_setup(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    Ok(Json(
        two_factor::begin_totp_setup(&state, auth.user_id).await?,
    ))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct TotpConfirmParams {
    /// The 6-digit code from the authenticator app.
    pub code: String,
}

/// `POST /api/v1/2fa/totp/confirm` — verify a code against the pending
/// secret, enable TOTP and return the first backup-code batch (shown
/// once).
#[utoipa::path(
    post,
    path = "/api/v1/2fa/totp/confirm",
    tag = "two_factor",
    request_body = TotpConfirmParams,
    responses(
        (status = 200, description = "TOTP enabled; backup codes shown once"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No pending TOTP setup", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid verification code", body = crate::error::ErrorBody),
    )
)]
pub async fn totp_confirm(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<TotpConfirmParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    let codes = two_factor::confirm_totp(&state, auth.user_id, &params.code).await?;
    Ok(Json(json!({ "backup_codes": codes })))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PasswordParams {
    /// The account password (re-authentication).
    pub password: String,
}

/// `DELETE /api/v1/2fa/totp` — remove TOTP. Backup codes are dropped
/// with it when no WebAuthn keys remain.
#[utoipa::path(
    delete,
    path = "/api/v1/2fa/totp",
    tag = "two_factor",
    request_body = PasswordParams,
    responses(
        (status = 200, description = "TOTP removed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Password mismatch", body = crate::error::ErrorBody),
        (status = 404, description = "TOTP not enabled", body = crate::error::ErrorBody),
    )
)]
pub async fn totp_delete(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<PasswordParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    two_factor::require_password(&state.db, auth.user_id, &params.password).await?;
    if !two_factor::disable_totp(&state, auth.user_id).await? {
        return Err(Error::NotFound("TOTP enrollment".into()));
    }
    Ok(Json(json!({})))
}

/// `POST /api/v1/2fa/backup_codes` — regenerate the batch; the
/// plaintext codes are shown exactly once.
#[utoipa::path(
    post,
    path = "/api/v1/2fa/backup_codes",
    tag = "two_factor",
    request_body = PasswordParams,
    responses(
        (status = 200, description = "New backup-code batch (shown once)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Password mismatch", body = crate::error::ErrorBody),
        (status = 422, description = "2FA not enabled", body = crate::error::ErrorBody),
    )
)]
pub async fn backup_codes(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<PasswordParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    two_factor::require_password(&state.db, auth.user_id, &params.password).await?;
    let codes = two_factor::regenerate_backup_codes(&state, auth.user_id).await?;
    Ok(Json(json!({ "backup_codes": codes })))
}

/// `POST /api/v1/2fa/webauthn/register/begin` — creation options for a
/// new security key; the ceremony state is staged server-side.
#[utoipa::path(
    post,
    path = "/api/v1/2fa/webauthn/register/begin",
    tag = "two_factor",
    responses(
        (status = 200, description = "WebAuthn creation options"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
    )
)]
pub async fn webauthn_register_begin(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    Ok(Json(
        webauthn::begin_registration(&state, auth.user_id).await?,
    ))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct WebauthnRegisterParams {
    /// Friendly name for the key (`""` → unnamed).
    pub name: Option<String>,
    /// The `RegisterPublicKeyCredential` from the user agent.
    pub credential: Value,
}

/// `POST /api/v1/2fa/webauthn/register` — finish the ceremony and
/// persist the named credential.
#[utoipa::path(
    post,
    path = "/api/v1/2fa/webauthn/register",
    tag = "two_factor",
    request_body = WebauthnRegisterParams,
    responses(
        (status = 200, description = "Credential registered"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No pending ceremony", body = crate::error::ErrorBody),
        (status = 409, description = "Key already registered", body = crate::error::ErrorBody),
        (status = 422, description = "Attestation rejected or invalid name", body = crate::error::ErrorBody),
    )
)]
pub async fn webauthn_register(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<WebauthnRegisterParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    let credential: webauthn_rs::prelude::RegisterPublicKeyCredential =
        serde_json::from_value(params.credential)
            .map_err(|e| Error::Validation(format!("malformed credential: {e}")))?;
    let name = params.name.unwrap_or_default();
    let id = webauthn::finish_registration(&state, auth.user_id, &name, &credential).await?;
    Ok(Json(json!({ "id": id.to_string(), "name": name })))
}

/// `GET /api/v1/2fa/webauthn/credentials` — the caller's registered
/// security keys.
#[utoipa::path(
    get,
    path = "/api/v1/2fa/webauthn/credentials",
    tag = "two_factor",
    responses(
        (status = 200, description = "Registered credentials"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn webauthn_credentials(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read")?;
    Ok(Json(Value::Array(
        webauthn::list_credentials(&state.db, auth.user_id).await?,
    )))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CredentialRenameParams {
    pub name: String,
}

/// `POST /api/v1/2fa/webauthn/credentials/{id}/rename`.
#[utoipa::path(
    post,
    path = "/api/v1/2fa/webauthn/credentials/{id}/rename",
    tag = "two_factor",
    params(("id" = i64, Path, description = "Credential id")),
    request_body = CredentialRenameParams,
    responses(
        (status = 200, description = "Credential renamed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Credential not found", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid name", body = crate::error::ErrorBody),
    )
)]
pub async fn webauthn_rename(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(params): Json<CredentialRenameParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    if !webauthn::rename_credential(&state.db, auth.user_id, id, &params.name).await? {
        return Err(Error::NotFound("credential".into()));
    }
    Ok(Json(json!({})))
}

/// `DELETE /api/v1/2fa/webauthn/credentials/{id}` — remove one key;
/// backup codes are dropped when no second factor remains.
#[utoipa::path(
    delete,
    path = "/api/v1/2fa/webauthn/credentials/{id}",
    tag = "two_factor",
    params(("id" = i64, Path, description = "Credential id")),
    request_body = PasswordParams,
    responses(
        (status = 200, description = "Credential removed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Password mismatch", body = crate::error::ErrorBody),
        (status = 404, description = "Credential not found", body = crate::error::ErrorBody),
    )
)]
pub async fn webauthn_delete(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(params): Json<PasswordParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    two_factor::require_password(&state.db, auth.user_id, &params.password).await?;
    if !two_factor::disable_webauthn_credential(&state, auth.user_id, id).await? {
        return Err(Error::NotFound("credential".into()));
    }
    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct DisableParams {
    /// The account password (re-authentication).
    pub password: String,
    /// Retain the backup-code batch after every method is gone —
    /// the deliberate "codes still usable" escape hatch.
    pub keep_backup_codes: Option<bool>,
}

/// `DELETE /api/v1/2fa` — disable every second factor. Backup codes
/// are deleted unless `keep_backup_codes` is set; issued tokens are
/// unaffected (2FA gates login, not token validity).
#[utoipa::path(
    delete,
    path = "/api/v1/2fa",
    tag = "two_factor",
    request_body = DisableParams,
    responses(
        (status = 200, description = "Two-factor disabled"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Password mismatch", body = crate::error::ErrorBody),
    )
)]
pub async fn disable(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<DisableParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    two_factor::require_password(&state.db, auth.user_id, &params.password).await?;
    two_factor::disable_all(
        &state,
        auth.user_id,
        params.keep_backup_codes.unwrap_or(false),
    )
    .await?;
    Ok(Json(json!({})))
}
