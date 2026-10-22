//! Root `/oauth/*` routes — token endpoint, authorization-code issue,
//! and the device flow. Registered outside `/api` in `build_router`
//! with the `authentication` rate-limit bucket.

use std::sync::Arc;

use axum::{
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{
        oauth::{self, invalid_client, invalid_grant, invalid_request, unsupported_grant_type},
        tokens::{self, SessionMeta},
    },
    http::{middleware::auth::AuthContext, middleware::client_ip::ClientIp, AppState},
    Result,
};

/// The `/oauth` subtree.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/oauth/token", post(token))
        .route("/oauth/authorize", get(authorize_info).post(authorize))
        .route("/oauth/device_code", post(device_code))
        .route("/oauth/device", get(device_info))
        .route("/oauth/device/authorize", post(device_authorize))
        .route_layer(crate::http::middleware::ratelimit::layer(
            state,
            "authentication",
        ))
}

#[derive(Debug, Deserialize)]
pub struct TokenParams {
    grant_type: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    username: Option<String>,
    password: Option<String>,
    scope: Option<String>,
    redirect_uri: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    device_code: Option<String>,
}

#[derive(serde::Serialize)]
struct TokenResponse(Value);
impl IntoResponse for TokenResponse {
    fn into_response(self) -> axum::response::Response {
        Json(self.0).into_response()
    }
}

/// `POST /oauth/token` — password, client_credentials,
/// authorization_code, refresh_token and device_code grants.
/// Errors use the doorkeeper JSON shape (`{"error","error_description"}`).
async fn token(
    State(state): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(mut params): Form<TokenParams>,
) -> std::result::Result<TokenResponse, crate::auth::oauth::GrantError> {
    // RFC 6749 §2.3.1: client credentials may arrive as HTTP Basic
    // (`Authorization: Basic base64(client_id:client_secret)`); the form
    // fields are the fallback.
    let (client_id, client_secret) = match basic_client_auth(&headers) {
        Some((id, secret)) => (Some(id), Some(secret)),
        None => (params.client_id.take(), params.client_secret.take()),
    };
    let client =
        oauth::authenticate_client(&state.db, client_id.as_deref(), client_secret.as_deref())
            .await?;
    match params.grant_type.as_str() {
        "password" => password_grant(&state, params, client, &headers, &ip).await,
        "client_credentials" => client_credentials_grant(&state, client).await,
        "authorization_code" => authorization_code_grant(&state, params, client).await,
        "refresh_token" => refresh_grant(&state, params, client).await,
        "urn:ietf:params:oauth:grant-type:device_code" => {
            device_code_grant(&state, params, client).await
        }
        _ => Err(unsupported_grant_type()),
    }
    .map(TokenResponse)
}

/// `Authorization: Basic base64(client_id:client_secret)` — the values
/// are form-encoded inside the blob per RFC 6749 §2.3.1.
fn basic_client_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, blob) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .ok()?;
    let pair = String::from_utf8(decoded).ok()?;
    let (id, secret) = pair.split_once(':')?;
    let decode = |s: &str| -> Option<String> {
        url::form_urlencoded::parse(s.as_bytes())
            .next()
            .map(|(k, _)| k.into_owned())
    };
    Some((decode(id)?, decode(secret)?))
}

fn requested_scope(
    requested: Option<&str>,
    app: Option<&oauth::AppRow>,
) -> std::result::Result<String, crate::auth::oauth::GrantError> {
    let requested = requested.unwrap_or_default();
    if requested.trim().is_empty() {
        return Ok(app
            .map(|a| a.scopes.clone())
            .unwrap_or_else(|| "read write".to_string()));
    }
    let scopes = crate::auth::scopes::ScopeSet::parse(requested)
        .map_err(|e| crate::auth::oauth::invalid_scope(&e.to_string()))?;
    if let Some(app) = app {
        let app_scopes = crate::auth::scopes::ScopeSet::parse(&app.scopes)
            .map_err(|e| crate::auth::oauth::GrantError::server(&e.to_string()))?;
        if !scopes.is_subset_of(&app_scopes) {
            return Err(oauth::invalid_scope(
                "The requested scope is outside the application's scopes.",
            ));
        }
    }
    Ok(scopes.as_string())
}

async fn password_grant(
    state: &Arc<AppState>,
    params: TokenParams,
    client: Option<oauth::AppRow>,
    headers: &HeaderMap,
    ip: &str,
) -> std::result::Result<Value, oauth::GrantError> {
    let username = params.username.unwrap_or_default();
    let password = params.password.unwrap_or_default();
    if username.is_empty() || password.is_empty() {
        return Err(invalid_request("Missing username or password."));
    }
    // Mastodon uses the email as `username`; accept a plain username too.
    let row = sqlx::query(
        "SELECT u.id, u.password_hash, a.suspended_at \
         FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE (lower(u.email) = lower($1) OR (lower(a.username) = lower($1) AND a.domain IS NULL))",
    )
    .bind(username.trim())
    .fetch_optional(&state.db)
    .await
    .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    let Some(row) = row else {
        return Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        ));
    };
    use sqlx::Row;
    if row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("suspended_at")
        .is_some()
        || !crate::auth::password::verify_password(&password, row.get("password_hash"))
    {
        return Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        ));
    }
    let user_id: i64 = row.get("id");
    let scope = requested_scope(params.scope.as_deref(), client.as_ref())?;

    // Interactive login → linked session row.
    let meta = SessionMeta {
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        ip: ip.parse::<std::net::IpAddr>().ok().map(|_| ip.to_string()),
    };
    let issued = tokens::issue_token_pair(
        state,
        client.as_ref().map(|a| a.id),
        user_id,
        &scope,
        None,
        Some(&meta),
    )
    .await
    .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    if let Some(app) = &client {
        tokens::record_authorization(&state.db, user_id, app.id, &scope)
            .await
            .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    }
    Ok(tokens::token_entity(&issued))
}

async fn client_credentials_grant(
    state: &Arc<AppState>,
    client: Option<oauth::AppRow>,
) -> std::result::Result<Value, oauth::GrantError> {
    let Some(client) = client else {
        return Err(invalid_client("Client authentication failed."));
    };
    let issued = tokens::issue_app_token(state, client.id, &client.scopes)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    Ok(tokens::token_entity(&issued))
}

async fn authorization_code_grant(
    state: &Arc<AppState>,
    params: TokenParams,
    client: Option<oauth::AppRow>,
) -> std::result::Result<Value, oauth::GrantError> {
    let Some(client) = client else {
        return Err(invalid_client("Client authentication failed."));
    };
    let code = params.code.unwrap_or_default();
    let redirect_uri = params.redirect_uri.unwrap_or_default();
    let payload = oauth::consume_auth_code(
        state,
        &client,
        &code,
        &redirect_uri,
        params.code_verifier.as_deref(),
    )
    .await?;
    let issued = tokens::issue_token_pair(
        state,
        Some(client.id),
        payload.user_id,
        &payload.scope,
        None,
        None,
    )
    .await
    .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    tokens::record_authorization(&state.db, payload.user_id, client.id, &payload.scope)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    Ok(tokens::token_entity(&issued))
}

async fn refresh_grant(
    state: &Arc<AppState>,
    params: TokenParams,
    client: Option<oauth::AppRow>,
) -> std::result::Result<Value, oauth::GrantError> {
    let raw = params.refresh_token.unwrap_or_default();
    if raw.is_empty() {
        return Err(invalid_request("Missing refresh_token."));
    }
    let issued = tokens::rotate_refresh_token(state, &raw, client.as_ref()).await?;
    Ok(tokens::token_entity(&issued))
}

async fn device_code_grant(
    state: &Arc<AppState>,
    params: TokenParams,
    client: Option<oauth::AppRow>,
) -> std::result::Result<Value, oauth::GrantError> {
    let Some(client) = client else {
        return Err(invalid_client("Client authentication failed."));
    };
    let code = params.device_code.unwrap_or_default();
    if code.is_empty() {
        return Err(invalid_request("Missing device_code."));
    }
    let entry = oauth::poll_device_code(state, &client, &code).await?;
    let user_id = entry
        .user_id
        .ok_or_else(|| invalid_grant("The device code is invalid."))?;
    let scope = entry.granted_scope.unwrap_or(entry.scope);
    let issued = tokens::issue_token_pair(state, Some(client.id), user_id, &scope, None, None)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    tokens::record_authorization(&state.db, user_id, client.id, &scope)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    Ok(tokens::token_entity(&issued))
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    client_id: String,
    redirect_uri: String,
    response_type: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

/// `GET /oauth/authorize` — consent metadata for the future UI.
async fn authorize_info(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<AuthorizeParams>,
) -> Result<Json<Value>> {
    let app = oauth::load_client(&state.db, &params.client_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("client".into()))?;
    Ok(Json(json!({
        "client_name": app.name,
        "website": app.website,
        "scopes": app.scopes.split_whitespace().collect::<Vec<_>>(),
        "redirect_uri": params.redirect_uri,
    })))
}

/// `POST /oauth/authorize` — an authenticated user issues a code for a
/// client. The consent UI lands with the frontend epics.
async fn authorize(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Form(params): Form<AuthorizeParams>,
) -> Result<Json<Value>> {
    if params.response_type.as_deref() != Some("code") {
        return Err(crate::Error::Validation(
            "response_type must be 'code'".into(),
        ));
    }
    let app = oauth::load_client(&state.db, &params.client_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("client".into()))?;
    let (code, redirect_to) = oauth::create_auth_code(
        &state,
        auth.user_id,
        &app,
        &oauth::AuthCodeRequest {
            redirect_uri: &params.redirect_uri,
            scope: params.scope.as_deref().unwrap_or_default(),
            code_challenge: params.code_challenge.as_deref(),
            code_challenge_method: params.code_challenge_method.as_deref(),
            state: params.state.as_deref(),
        },
    )
    .await
    .map_err(|e| crate::Error::Validation(e.description))?;
    Ok(Json(json!({
        "code": code,
        "state": params.state,
        "redirect_to": redirect_to,
    })))
}

#[derive(Debug, Deserialize)]
pub struct DeviceCodeParams {
    client_id: String,
    client_secret: Option<String>,
    scope: Option<String>,
}

/// `POST /oauth/device_code` — start a device authorization session.
async fn device_code(
    State(state): State<Arc<AppState>>,
    Form(params): Form<DeviceCodeParams>,
) -> std::result::Result<Json<Value>, oauth::GrantError> {
    let client = oauth::authenticate_client(
        &state.db,
        Some(&params.client_id),
        params.client_secret.as_deref(),
    )
    .await?
    .ok_or_else(|| invalid_client("Client authentication failed."))?;
    let body =
        oauth::create_device_code(&state, &client, params.scope.as_deref().unwrap_or_default())
            .await?;
    Ok(Json(body))
}

/// `GET /oauth/device` — JSON instructions for the API-only approval
/// path (the browser consent page lands with the frontend epics).
async fn device_info() -> Json<Value> {
    Json(json!({
        "approve": "POST /oauth/device/authorize with a user bearer token and {\"user_code\": \"XXXX-XXXX\"}",
    }))
}

#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizeParams {
    user_code: String,
    scope: Option<String>,
}

/// `POST /oauth/device/authorize` — an authenticated user approves a
/// device `user_code`.
async fn device_authorize(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Form(params): Form<DeviceAuthorizeParams>,
) -> Result<Json<Value>> {
    if !oauth::approve_device_code(
        &state,
        auth.user_id,
        &params.user_code,
        params.scope.as_deref(),
    )
    .await?
    {
        return Err(crate::Error::NotFound("device code".into()));
    }
    Ok(Json(json!({ "approved": true })))
}
