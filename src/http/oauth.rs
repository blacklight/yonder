//! Root `/oauth/*` routes — token endpoint, authorization-code issue,
//! and the device flow. Registered outside `/api` in `build_router`
//! with the `authentication` rate-limit bucket.

use std::sync::Arc;

use axum::{
    extract::{FromRequest, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{
        oauth::{self, invalid_client, invalid_grant, invalid_request, unsupported_grant_type},
        tokens::{self, SessionMeta},
        two_factor, webauthn,
    },
    http::{middleware::auth::AuthContext, middleware::client_ip::ClientIp, AppState},
    Result,
};

/// The `/oauth` subtree.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/oauth/token", post(token))
        .route("/oauth/authorize", post(authorize))
        // Some clients (toot) hit the authorize URL with a trailing
        // slash; axum matches paths exactly, so without these aliases
        // the request misses the route and the SPA fallback answers a
        // JSON 404 for the `/oauth/*` prefix.
        .route("/oauth/authorize/", post(authorize))
        .route("/oauth/device_code", post(device_code))
        .route("/oauth/register", post(register))
        .route("/oauth/device", get(device_info))
        .route("/oauth/device/authorize", post(device_authorize))
        // 2FA exchange endpoints share the `authentication` bucket.
        .route("/oauth/2fa/verify", post(two_factor_verify))
        .route(
            "/oauth/2fa/webauthn/assert/begin",
            post(two_factor_webauthn_begin),
        )
        .route(
            "/oauth/2fa/webauthn/assert",
            post(two_factor_webauthn_assert),
        )
        .route_layer(crate::http::middleware::ratelimit::layer(
            state,
            "authentication",
        ))
        // Cheap public read — the consent page's own metadata fetch
        // and every browser navigation would otherwise drain the tiny
        // `authentication` bucket and 429 the flow mid-consent.
        .route("/oauth/authorize", get(authorize_info))
        .route("/oauth/authorize/", get(authorize_info))
        // RFC 8414 authorization-server metadata — ActivityPub C2S
        // clients (Fedifox) discover the OAuth endpoints here when the
        // actor document carries no `oauth*Endpoint` fields (pubby
        // doesn't emit them).
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
}

/// `/oauth/*` clients split on body encoding: browsers and most
/// libraries post `x-www-form-urlencoded`, while several Mastodon
/// apps (Moshidon among them) post `application/json`. Doorkeeper
/// accepts both; deserialize whichever the `Content-Type` declares,
/// defaulting to form.
pub(crate) async fn body_params<T: serde::de::DeserializeOwned>(
    req: Request,
    state: &Arc<AppState>,
) -> std::result::Result<T, crate::Error> {
    let is_json = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| v)
            .map_err(|e| crate::Error::BadRequest(e.body_text()))
    } else {
        Form::<T>::from_request(req, state)
            .await
            .map(|Form(v)| v)
            .map_err(|e| crate::Error::BadRequest(e.body_text()))
    }
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
    req: Request,
) -> std::result::Result<TokenResponse, crate::auth::oauth::GrantError> {
    let mut params: TokenParams = body_params(req, &state)
        .await
        .map_err(|_| invalid_request("The request body is malformed."))?;
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
        // App-less grants match anonymous registration: `push` is
        // included so first-party clients can subscribe to Web Push
        // without an OAuth app round-trip.
        return Ok(app
            .map(|a| a.scopes.clone())
            .unwrap_or_else(|| "read write push".to_string()));
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
    // Disabled, deleted and pending-deletion accounts stay locked out;
    // a suspended account may still log in but only receives a
    // restricted (export-only) credential.
    let row = sqlx::query(
        "SELECT u.id, u.password_hash, a.suspended_at, a.deleted_at, \
                EXISTS(SELECT 1 FROM account_deletions d \
                       WHERE d.account_id = a.id AND d.state IN ('pending','running')) \
                    AS deleting \
         FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE (lower(u.email) = lower($1) OR (lower(a.username) = lower($1) AND a.domain IS NULL)) \
           AND a.disabled_at IS NULL",
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
    let password_hash: String = row.get("password_hash");
    if !crate::auth::password::verify_password_async(&password, &password_hash).await {
        return Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        ));
    }
    if row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("deleted_at")
        .is_some()
        || row.get::<bool, _>("deleting")
    {
        return Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        ));
    }
    let restricted = row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("suspended_at")
        .is_some();
    let user_id: i64 = row.get("id");
    let scope = if restricted {
        "read:backups".to_string()
    } else {
        requested_scope(params.scope.as_deref(), client.as_ref())?
    };

    // 2FA gate: an enrolled account either finishes inline (a `code`
    // form param — TOTP first, then backup code) or gets an
    // `mfa_required` answer carrying a pending-login token for the
    // `/oauth/2fa/*` exchange endpoints. Inline attempts aren't counted
    // against an attempt budget — each one re-verifies the argon2id
    // password — only the exchange endpoints carry a budget.
    let twofa = two_factor::two_factor_state(&state.db, user_id)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    if twofa.enabled() {
        match params
            .code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            Some(code) => {
                let ok = two_factor::verify_totp(state, user_id, code)
                    .await
                    .map_err(|e| oauth::GrantError::server(&e.to_string()))?
                    || two_factor::consume_backup_code(&state.db, user_id, code)
                        .await
                        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
                if !ok {
                    return Err(invalid_grant("The provided two-factor code is invalid."));
                }
            }
            None => {
                let mfa_token = two_factor::stage_pending_login(
                    state,
                    user_id,
                    client.as_ref().map(|a| a.id),
                    &scope,
                )
                .await
                .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
                return Err(oauth::mfa_required(&mfa_token, &twofa.methods()));
            }
        }
    }

    // Interactive login → linked session row.
    let meta = SessionMeta {
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        ip: ip.parse::<std::net::IpAddr>().ok().map(|_| ip.to_string()),
    };
    issue_credentials(
        state,
        user_id,
        client.as_ref().map(|a| a.id),
        &scope,
        Some(&meta),
        restricted,
    )
    .await
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
    let suspended = account_gate(&state.db, payload.user_id).await?;
    issue_credentials(
        state,
        payload.user_id,
        Some(client.id),
        &payload.scope,
        None,
        suspended,
    )
    .await
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
    let suspended = account_gate(&state.db, user_id).await?;
    let scope = entry.granted_scope.unwrap_or(entry.scope);
    issue_credentials(state, user_id, Some(client.id), &scope, None, suspended).await
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

/// `GET /oauth/authorize` — browsers opening the authorization URL
/// (`Accept: text/html`, the client app's consent hop) get the SPA so
/// its authorize page can mount; API callers — including that page —
/// get the consent metadata JSON.
async fn authorize_info(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<AuthorizeParams>,
) -> Result<Response> {
    let wants_html = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if wants_html {
        return Ok(crate::http::frontend::spa_index(&state));
    }
    let app = oauth::load_client(&state.db, &params.client_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("client".into()))?;
    Ok(Json(json!({
        "client_name": app.name,
        "website": app.website,
        "scopes": app.scopes.split_whitespace().collect::<Vec<_>>(),
        "redirect_uri": params.redirect_uri,
    }))
    .into_response())
}

/// `POST /oauth/authorize` — an authenticated user issues a code for a
/// client. The SPA's authorize page calls this with the session token
/// and navigates to the returned `redirect_to`.
async fn authorize(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    req: Request,
) -> Result<Json<Value>> {
    let params: AuthorizeParams = body_params(req, &state).await?;
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

/// `GET /.well-known/oauth-authorization-server` — RFC 8414
/// authorization-server metadata. This is how ActivityPub C2S clients
/// (Fedifox) find the authorize/token/registration endpoints when the
/// actor document doesn't carry `oauth*Endpoint` fields.
async fn server_metadata(State(state): State<Arc<AppState>>) -> Json<Value> {
    let base = crate::activitypub::federation_base_url(state.config().as_ref());
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": crate::auth::scopes::KNOWN_SCOPES,
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": [
            "authorization_code",
            "refresh_token",
            "client_credentials",
            "password",
            "urn:ietf:params:oauth:grant-type:device_code",
        ],
        "code_challenge_methods_supported": ["S256", "plain"],
        "token_endpoint_auth_methods_supported": [
            "client_secret_post",
            "client_secret_basic",
            "none",
        ],
    }))
}

/// RFC 7591 dynamic client registration body. Unknown members
/// (`software_id`, `software_version`, `client_uri`, …) are ignored.
#[derive(Debug, Deserialize)]
pub struct RegisterParams {
    /// Required by RFC 7591 — the redirect targets the client will
    /// use (web extensions register `moz-extension://`/`https://*.mozilla.org`).
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    scope: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    client_uri: Option<String>,
}

/// Grant types this server actually issues tokens for.
const REGISTER_GRANT_TYPES: &[&str] = &[
    "authorization_code",
    "refresh_token",
    "client_credentials",
    "password",
    "urn:ietf:params:oauth:grant-type:device_code",
];

/// `POST /oauth/register` — RFC 7591 dynamic client registration.
/// `token_endpoint_auth_method: "none"` yields a public client
/// (PKCE-only, no secret); anything else registers a confidential
/// client whose secret is returned exactly once.
async fn register(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> std::result::Result<Response, RegisterError> {
    let params: RegisterParams = Json::<RegisterParams>::from_request(req, &state)
        .await
        .map_err(|e| RegisterError::invalid(&e.body_text()))?
        .0;
    if params.redirect_uris.is_empty() {
        return Err(RegisterError::invalid("redirect_uris is required"));
    }
    if let Some(grants) = &params.grant_types {
        if grants
            .iter()
            .any(|g| !REGISTER_GRANT_TYPES.contains(&g.as_str()))
        {
            return Err(RegisterError::invalid(
                "unsupported grant_type in grant_types",
            ));
        }
    }
    if let Some(types) = &params.response_types {
        if types.iter().any(|t| t != "code") {
            return Err(RegisterError::invalid(
                "unsupported response_type in response_types",
            ));
        }
    }
    let auth_method = params.token_endpoint_auth_method.as_deref();
    if let Some(m) = auth_method {
        if !matches!(m, "none" | "client_secret_post" | "client_secret_basic") {
            return Err(RegisterError::invalid(
                "unsupported token_endpoint_auth_method",
            ));
        }
    }
    let confidential = auth_method != Some("none");
    let name = params
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Registered client")
        .to_string();
    // RFC 7591: an omitted `scope` means "all scopes the client may
    // use". C2S clients need `write`; default to the interactive set
    // rather than the read-only `/api/v1/apps` default.
    let scopes = params
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("read write")
        .to_string();
    let (app, secret) = oauth::create_app(
        &state,
        &name,
        &params.redirect_uris,
        &scopes,
        params.client_uri.as_deref(),
        confidential,
        None,
    )
    .await
    .map_err(|e| match e {
        crate::Error::Validation(msg) => RegisterError::invalid(&msg),
        other => RegisterError::server(&other.to_string()),
    })?;

    let mut body = json!({
        "client_id": app.client_id,
        "client_id_issued_at": chrono::Utc::now().timestamp(),
        "redirect_uris": app.redirect_uris,
        "token_endpoint_auth_method": if confidential { "client_secret_post" } else { "none" },
        "grant_types": params.grant_types.unwrap_or_else(|| vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ]),
        "response_types": ["code"],
        "client_name": app.name,
        "scope": app.scopes,
    });
    if confidential {
        body["client_secret"] = json!(secret);
        body["client_secret_expires_at"] = json!(0);
    }
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// RFC 7591 §3.2.2 error shape (`{"error", "error_description"}`).
struct RegisterError {
    status: StatusCode,
    code: &'static str,
    description: String,
}

impl RegisterError {
    fn invalid(description: &str) -> Self {
        RegisterError {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_client_metadata",
            description: description.to_string(),
        }
    }

    fn server(description: &str) -> Self {
        RegisterError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "server_error",
            description: description.to_string(),
        }
    }
}

impl IntoResponse for RegisterError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": self.code,
                "error_description": self.description,
            })),
        )
            .into_response()
    }
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
    req: Request,
) -> std::result::Result<Json<Value>, oauth::GrantError> {
    let params: DeviceCodeParams = body_params(req, &state)
        .await
        .map_err(|_| invalid_request("The request body is malformed."))?;
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
    req: Request,
) -> Result<Json<Value>> {
    let params: DeviceAuthorizeParams = body_params(req, &state).await?;
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

// ---------------------------------------------------------------------------
// Two-factor exchange (`/oauth/2fa/*`)
// ---------------------------------------------------------------------------

/// Management-API errors crossing into the doorkeeper shape: known
/// client problems (`NotFound`, `Validation`) become `invalid_grant`;
/// anything else is an infrastructure `server_error`.
fn grantify(e: crate::Error) -> oauth::GrantError {
    match e {
        crate::Error::NotFound(_) | crate::Error::Validation(_) => {
            invalid_grant("The provided authorization grant is invalid, expired or revoked.")
        }
        other => oauth::GrantError::server(&other.to_string()),
    }
}

/// Load the pending login for `mfa_token` — `invalid_grant` when it
/// expired or never existed.
async fn require_pending_login(
    state: &Arc<AppState>,
    mfa_token: &str,
) -> std::result::Result<two_factor::PendingLogin, oauth::GrantError> {
    two_factor::pending_login(state, mfa_token)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?
        .ok_or_else(|| {
            invalid_grant("The provided authorization grant is invalid, expired or revoked.")
        })
}

/// Count a failed second-factor attempt against the pending login and
/// answer `invalid_grant` — consistent with the wrong-password shape.
async fn failed_second_factor(state: &Arc<AppState>, mfa_token: &str) -> oauth::GrantError {
    if let Err(e) = two_factor::fail_pending_login(state, mfa_token).await {
        return oauth::GrantError::server(&e.to_string());
    }
    invalid_grant("The provided two-factor credential is invalid.")
}

/// The lifecycle gate every issuance path shares: `Ok(suspended)`
/// for a live account (suspension gates restricted issuance), `Err`
/// when the account is gone or unknown — disabled, deleted, pending
/// self-deletion and a missing user all lock out the same way.
async fn account_gate(
    db: &sqlx::PgPool,
    user_id: i64,
) -> std::result::Result<bool, oauth::GrantError> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT a.suspended_at IS NOT NULL, \
                a.disabled_at IS NOT NULL OR a.deleted_at IS NOT NULL OR \
                EXISTS(SELECT 1 FROM account_deletions d \
                       WHERE d.account_id = a.id AND d.state IN ('pending','running')) \
         FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
    .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    match row {
        Some((suspended, false)) => Ok(suspended),
        _ => Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        )),
    }
}

/// Issue the credential for an account that passed [`account_gate`]:
/// a restricted export-only token while suspended, otherwise an
/// ordinary pair recorded as an authorization for application-bound
/// grants. Returns the token entity.
async fn issue_credentials(
    state: &Arc<AppState>,
    user_id: i64,
    application_id: Option<i64>,
    scope: &str,
    session: Option<&SessionMeta>,
    suspended: bool,
) -> std::result::Result<Value, oauth::GrantError> {
    if suspended {
        let issued = tokens::issue_restricted_token(state, user_id, session)
            .await
            .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
        let mut entity = tokens::token_entity(&issued);
        entity["yonder"] = json!({ "restricted": true });
        return Ok(entity);
    }
    let issued = tokens::issue_token_pair(state, application_id, user_id, scope, None, session)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    if let Some(application_id) = application_id {
        tokens::record_authorization(&state.db, user_id, application_id, scope)
            .await
            .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    }
    Ok(tokens::token_entity(&issued))
}

/// Finish a completed pending login exactly like a successful
/// `password_grant`: token pair + session row (interactive login) +
/// `oauth_authorizations` upsert for client-bound grants. Account
/// state is re-checked at issuance: a suspension that lands between
/// the password and second-factor steps still yields only a
/// restricted credential, and deletion locks the account out.
async fn issue_for_pending_login(
    state: &Arc<AppState>,
    mfa_token: &str,
    headers: &HeaderMap,
    ip: &str,
) -> std::result::Result<Value, oauth::GrantError> {
    let pending = two_factor::consume_pending_login(state, mfa_token)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?
        .ok_or_else(|| {
            invalid_grant("The provided authorization grant is invalid, expired or revoked.")
        })?;
    let suspended = account_gate(&state.db, pending.user_id).await?;
    let meta = SessionMeta {
        user_agent: headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        ip: ip.parse::<std::net::IpAddr>().ok().map(|_| ip.to_string()),
    };
    issue_credentials(
        state,
        pending.user_id,
        pending.application_id,
        &pending.scope,
        Some(&meta),
        suspended,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct TwoFactorVerifyParams {
    mfa_token: String,
    code: String,
}

/// `POST /oauth/2fa/verify` — complete a pending login with a TOTP
/// code or a backup code (consumed on success).
async fn two_factor_verify(
    State(state): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    req: Request,
) -> std::result::Result<TokenResponse, oauth::GrantError> {
    let params: TwoFactorVerifyParams = body_params(req, &state)
        .await
        .map_err(|_| invalid_request("The request body is malformed."))?;
    let pending = require_pending_login(&state, &params.mfa_token).await?;
    let ok = two_factor::verify_totp(&state, pending.user_id, &params.code)
        .await
        .map_err(|e| oauth::GrantError::server(&e.to_string()))?
        || two_factor::consume_backup_code(&state.db, pending.user_id, &params.code)
            .await
            .map_err(|e| oauth::GrantError::server(&e.to_string()))?;
    if !ok {
        return Err(failed_second_factor(&state, &params.mfa_token).await);
    }
    issue_for_pending_login(&state, &params.mfa_token, &headers, &ip)
        .await
        .map(TokenResponse)
}

#[derive(Debug, Deserialize)]
pub struct TwoFactorMfaTokenParams {
    mfa_token: String,
}

/// `POST /oauth/2fa/webauthn/assert/begin` — assertion options for the
/// pending login's registered keys.
async fn two_factor_webauthn_begin(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> std::result::Result<Json<Value>, oauth::GrantError> {
    let params: TwoFactorMfaTokenParams = body_params(req, &state)
        .await
        .map_err(|_| invalid_request("The request body is malformed."))?;
    let pending = require_pending_login(&state, &params.mfa_token).await?;
    let options = webauthn::begin_authentication(&state, &params.mfa_token, pending.user_id)
        .await
        .map_err(grantify)?;
    Ok(Json(options))
}

#[derive(Debug, Deserialize)]
pub struct TwoFactorAssertParams {
    mfa_token: String,
    /// JSON-serialized `PublicKeyCredential` from the user agent.
    credential: String,
}

/// `POST /oauth/2fa/webauthn/assert` — verify the assertion and issue
/// the token pair.
async fn two_factor_webauthn_assert(
    State(state): State<Arc<AppState>>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    req: Request,
) -> std::result::Result<TokenResponse, oauth::GrantError> {
    let params: TwoFactorAssertParams = body_params(req, &state)
        .await
        .map_err(|_| invalid_request("The request body is malformed."))?;
    let pending = require_pending_login(&state, &params.mfa_token).await?;
    let credential: webauthn_rs::prelude::PublicKeyCredential =
        serde_json::from_str(&params.credential)
            .map_err(|_| invalid_grant("The credential payload is malformed."))?;
    let ok =
        webauthn::finish_authentication(&state, &params.mfa_token, pending.user_id, &credential)
            .await
            .map_err(grantify)?;
    if !ok {
        return Err(failed_second_factor(&state, &params.mfa_token).await);
    }
    issue_for_pending_login(&state, &params.mfa_token, &headers, &ip)
        .await
        .map(TokenResponse)
}
