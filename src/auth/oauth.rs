//! OAuth2: application registry, client authentication, the
//! authorization-code (+ PKCE) and device flows, and authorized-app
//! revocation.
//!
//! Grant failures use the doorkeeper JSON shape
//! (`{"error", "error_description"}`) via [`GrantError`] — deliberately
//! not `crate::Error`, whose body format is Yonder's own.

use axum::{http::StatusCode, response::IntoResponse, Json};
use base64::Engine;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{
    auth::{
        scopes::ScopeSet,
        tokens::{get_secret_key, random_secret, set_secret_key, take_secret_key},
    },
    http::AppState,
    Error, Result,
};

/// OAuth2 error response rendered as the doorkeeper/RFC 6749 JSON shape.
#[derive(Debug)]
pub struct GrantError {
    pub error: &'static str,
    pub description: String,
    pub status: StatusCode,
}

impl GrantError {
    pub fn server(description: &str) -> Self {
        GrantError {
            error: "server_error",
            description: description.to_string(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for GrantError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(json!({
                "error": self.error,
                "error_description": self.description,
            })),
        )
            .into_response()
    }
}

pub fn invalid_client(description: &str) -> GrantError {
    GrantError {
        error: "invalid_client",
        description: description.to_string(),
        status: StatusCode::UNAUTHORIZED,
    }
}

pub fn invalid_grant(description: &str) -> GrantError {
    GrantError {
        error: "invalid_grant",
        description: description.to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn invalid_request(description: &str) -> GrantError {
    GrantError {
        error: "invalid_request",
        description: description.to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn invalid_scope(description: &str) -> GrantError {
    GrantError {
        error: "invalid_scope",
        description: description.to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn unsupported_grant_type() -> GrantError {
    GrantError {
        error: "unsupported_grant_type",
        description: "The authorization grant type is not supported.".to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn authorization_pending() -> GrantError {
    GrantError {
        error: "authorization_pending",
        description: "The device authorization is still pending.".to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn slow_down() -> GrantError {
    GrantError {
        error: "slow_down",
        description: "Polling too fast — respect the interval.".to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

pub fn expired_token() -> GrantError {
    GrantError {
        error: "expired_token",
        description: "The device code has expired.".to_string(),
        status: StatusCode::BAD_REQUEST,
    }
}

/// An `oauth_applications` row used across the auth module.
#[derive(Debug, Clone)]
pub struct AppRow {
    pub id: i64,
    pub client_id: String,
    pub client_secret_hash: String,
    pub redirect_uris: Vec<String>,
    pub scopes: String,
    pub confidential: bool,
    pub name: String,
    pub website: Option<String>,
}

impl AppRow {
    /// First registered redirect URI — the Mastodon Application
    /// entity's `redirect_uri` field.
    pub fn redirect_uri(&self) -> String {
        self.redirect_uris.first().cloned().unwrap_or_default()
    }

    /// Application entity for `verify_credentials`/authorized lists.
    pub fn to_entity(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "name": self.name,
            "website": self.website,
            "redirect_uri": self.redirect_uri(),
            "client_id": self.client_id,
            "vapid_key": Value::Null,
        })
    }
}

pub async fn load_client(pool: &sqlx::PgPool, client_id: &str) -> Result<Option<AppRow>> {
    let row = sqlx::query(
        "SELECT id, client_id, client_secret_hash, redirect_uris, scopes, \
                confidential, name, website \
         FROM oauth_applications WHERE client_id = $1",
    )
    .bind(client_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AppRow {
        id: r.get("id"),
        client_id: r.get("client_id"),
        client_secret_hash: r.get("client_secret_hash"),
        redirect_uris: r.get("redirect_uris"),
        scopes: r.get("scopes"),
        confidential: r.get("confidential"),
        name: r.get("name"),
        website: r.get("website"),
    }))
}

/// Fixed-time sha256 comparison — secrets are stored as
/// `sha256(secret)`; both sides are fixed-length digests compared
/// without early exit.
pub fn verify_client_secret(app: &AppRow, client_secret: &str) -> bool {
    let digest = Sha256::digest(client_secret.as_bytes());
    let stored = hex::decode(&app.client_secret_hash).unwrap_or_default();
    stored.len() == digest.len()
        && stored
            .iter()
            .zip(digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Authenticate an OAuth client: missing `client_id` yields `None`
/// (anonymous grant). Confidential clients must supply a valid secret;
/// public clients must not supply one.
pub async fn authenticate_client(
    pool: &sqlx::PgPool,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> std::result::Result<Option<AppRow>, GrantError> {
    let Some(client_id) = client_id.filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let app = load_client(pool, client_id)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?
        .ok_or_else(|| invalid_client("Client authentication failed."))?;
    if app.confidential {
        let secret = client_secret.unwrap_or_default();
        if secret.is_empty() || !verify_client_secret(&app, secret) {
            return Err(invalid_client("Client authentication failed."));
        }
    } else if client_secret.is_some_and(|s| !s.is_empty()) {
        return Err(invalid_client(
            "Public clients must not send a client_secret.",
        ));
    }
    Ok(Some(app))
}

// ---------------------------------------------------------------------------
// Application registration
// ---------------------------------------------------------------------------

/// Out-of-band redirect targets Mastodon clients legitimately use.
const OOB_URIS: &[&str] = &[
    "urn:ietf:wg:oauth:2.0:oob",
    "urn:ietf:wg:oauth:2.0:oob:auto",
];
const MAX_REDIRECT_URI_LENGTH: usize = 512;
const MAX_APP_NAME_LENGTH: usize = 500;

fn is_loopback_host(host: &str) -> bool {
    let normalized = host.trim_matches(|c| c == '[' || c == ']').to_lowercase();
    if normalized == "localhost" {
        return true;
    }
    normalized
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Validate a redirect URI per the Songhive rules plus Mastodon's OOB
/// targets: `urn:` OOB placeholders, `https`, or `http` for
/// loopback/localhost only; no fragments.
fn validate_redirect_uri(uri: &str) -> Result<()> {
    if uri.len() > MAX_REDIRECT_URI_LENGTH {
        return Err(Error::Validation("redirect URI too long".to_string()));
    }
    if OOB_URIS.contains(&uri) {
        return Ok(());
    }
    let parsed = url::Url::parse(uri)
        .map_err(|_| Error::Validation(format!("invalid redirect URI '{uri}'")))?;
    match parsed.scheme() {
        "https" => {}
        "http" if parsed.host_str().is_some_and(is_loopback_host) => {}
        scheme => {
            return Err(Error::Validation(format!(
                "redirect URI scheme '{scheme}' not allowed (https, or http for loopback)"
            )))
        }
    }
    if parsed.fragment().is_some() {
        return Err(Error::Validation(
            "redirect URIs must not contain fragments".to_string(),
        ));
    }
    if parsed.domain().is_none() && parsed.host_str().is_none() {
        return Err(Error::Validation(format!("invalid redirect URI '{uri}'")));
    }
    Ok(())
}

/// `POST /api/v1/apps` — register an OAuth application. Returns the row
/// plus the plaintext client secret (shown exactly once).
pub async fn create_app(
    state: &AppState,
    name: &str,
    redirect_uris: &[String],
    scopes: &str,
    website: Option<&str>,
    confidential: bool,
    owner_account_id: Option<i64>,
) -> Result<(AppRow, String)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_APP_NAME_LENGTH {
        return Err(Error::Validation(
            "client_name is required (max 500 chars)".into(),
        ));
    }
    if redirect_uris.is_empty() {
        return Err(Error::Validation(
            "at least one redirect_uri is required".to_string(),
        ));
    }
    for uri in redirect_uris {
        validate_redirect_uri(uri)?;
    }
    let parsed_scopes = ScopeSet::parse(scopes)?;
    let scopes = if parsed_scopes.0.is_empty() {
        "read".to_string()
    } else {
        parsed_scopes.as_string()
    };

    let client_id = random_secret();
    let client_secret = random_secret();
    // Public clients can't authenticate by secret; the NOT NULL column
    // stores a random unissued hash that nothing can ever match.
    let secret_stored = if confidential {
        &client_secret
    } else {
        &client_id
    };
    let secret_hash = hex::encode(Sha256::digest(secret_stored.as_bytes()));

    let row = sqlx::query(
        "INSERT INTO oauth_applications \
         (name, client_id, client_secret_hash, redirect_uris, scopes, website, confidential, owner_account_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING id, created_at",
    )
    .bind(name)
    .bind(&client_id)
    .bind(&secret_hash)
    .bind(redirect_uris)
    .bind(&scopes)
    .bind(website)
    .bind(confidential)
    .bind(owner_account_id)
    .fetch_one(&state.db)
    .await?;

    let app = AppRow {
        id: row.get("id"),
        client_id,
        client_secret_hash: secret_hash,
        redirect_uris: redirect_uris.to_vec(),
        scopes,
        confidential,
        name: name.to_string(),
        website: website.map(String::from),
    };
    let returned_secret = if confidential {
        client_secret
    } else {
        String::new()
    };
    Ok((app, returned_secret))
}

// ---------------------------------------------------------------------------
// Authorization codes (+ PKCE)
// ---------------------------------------------------------------------------

const AUTH_CODE_TTL_SECONDS: u64 = 600;
const DEVICE_CODE_TTL_SECONDS: u64 = 900;
const DEVICE_POLL_INTERVAL_SECONDS: u64 = 5;

/// Redis-persisted authorization-code payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthCodePayload {
    pub application_id: i64,
    pub redirect_uri: String,
    pub user_id: i64,
    pub scope: String,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

/// The per-request parameters of `POST /oauth/authorize`.
pub struct AuthCodeRequest<'a> {
    pub redirect_uri: &'a str,
    pub scope: &'a str,
    pub code_challenge: Option<&'a str>,
    pub code_challenge_method: Option<&'a str>,
    /// Opaque client state echoed back in `redirect_to`.
    pub state: Option<&'a str>,
}

/// `POST /oauth/authorize` — issue a code bound to (user, app,
/// redirect_uri, PKCE challenge). Public clients must PKCE.
pub async fn create_auth_code(
    state: &AppState,
    user_id: i64,
    app: &AppRow,
    req: &AuthCodeRequest<'_>,
) -> std::result::Result<(String, String), GrantError> {
    if !app.redirect_uris.iter().any(|u| u == req.redirect_uri) {
        return Err(invalid_request("The redirect uri included is not valid."));
    }
    let requested = ScopeSet::parse(req.scope)
        .map_err(|e| invalid_scope(&format!("The requested scope is invalid: {e}")))?;
    let app_scopes =
        ScopeSet::parse(&app.scopes).map_err(|e| GrantError::server(&e.to_string()))?;
    if !requested.0.is_empty() && !requested.is_subset_of(&app_scopes) {
        return Err(invalid_scope(
            "The requested scope is outside the application's scopes.",
        ));
    }
    if !app.confidential && req.code_challenge.is_none() {
        return Err(invalid_request(
            "Public clients must send a PKCE code_challenge.",
        ));
    }
    let method = req.code_challenge_method.unwrap_or("S256");
    if req.code_challenge.is_some() && !matches!(method, "S256" | "plain") {
        return Err(invalid_request("Unsupported code_challenge_method."));
    }

    let code = random_secret();
    let payload = AuthCodePayload {
        application_id: app.id,
        redirect_uri: req.redirect_uri.to_string(),
        user_id,
        scope: if requested.0.is_empty() {
            app.scopes.clone()
        } else {
            requested.as_string()
        },
        code_challenge: req.code_challenge.map(String::from),
        code_challenge_method: req.code_challenge.map(|_| method.to_string()),
    };
    set_secret_key(
        state,
        "oauth-authz",
        &code,
        &json!(payload),
        AUTH_CODE_TTL_SECONDS,
    )
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;

    let sep = if req.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut redirect_to = format!("{}{sep}code={code}", req.redirect_uri);
    if let Some(auth_state) = req.state {
        let encoded: String = url::form_urlencoded::byte_serialize(auth_state.as_bytes()).collect();
        redirect_to.push_str(&format!("&state={encoded}"));
    }
    Ok((code, redirect_to))
}

/// Consume an authorization code (single-use GETDEL), validate the
/// client + redirect_uri + PKCE verifier, and return the payload.
pub async fn consume_auth_code(
    state: &AppState,
    app: &AppRow,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> std::result::Result<AuthCodePayload, GrantError> {
    let payload: Option<AuthCodePayload> = take_secret_key(state, "oauth-authz", code)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    let Some(payload) = payload else {
        return Err(invalid_grant(
            "The provided authorization grant is invalid.",
        ));
    };
    if payload.application_id != app.id || payload.redirect_uri != redirect_uri {
        return Err(invalid_grant(
            "The provided authorization grant is invalid, expired or revoked.",
        ));
    }
    if let Some(challenge) = &payload.code_challenge {
        let verifier = code_verifier
            .ok_or_else(|| invalid_grant("Missing code_verifier for a PKCE grant."))?;
        let valid = match payload.code_challenge_method.as_deref() {
            Some("plain") => verifier == challenge,
            _ => {
                // S256: base64url(sha256(verifier)) without padding.
                let digest = Sha256::digest(verifier.as_bytes());
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest) == *challenge
            }
        };
        if !valid {
            return Err(invalid_grant(
                "The code_verifier does not match the challenge.",
            ));
        }
    }
    Ok(payload)
}

// ---------------------------------------------------------------------------
// Device flow (RFC 8628, API-only approval)
// ---------------------------------------------------------------------------

/// Redis-persisted device-flow entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceCodeEntry {
    pub application_id: i64,
    pub scope: String,
    pub user_code: String,
    /// `pending` or `granted`.
    pub status: String,
    pub user_id: Option<i64>,
    pub granted_scope: Option<String>,
}

fn device_domain() -> &'static str {
    "oauth-device"
}
fn user_code_domain() -> &'static str {
    "oauth-device-user"
}
/// `SET NX EX` marker gating polls to one per interval (atomic — no
/// read-modify-write on the entry itself).
fn device_poll_domain() -> &'static str {
    "oauth-device-poll"
}

/// `POST /oauth/device_code` — start a device authorization session.
pub async fn create_device_code(
    state: &AppState,
    app: &AppRow,
    scope: &str,
) -> std::result::Result<Value, GrantError> {
    let requested = ScopeSet::parse(scope)
        .map_err(|e| invalid_scope(&format!("The requested scope is invalid: {e}")))?;
    let app_scopes =
        ScopeSet::parse(&app.scopes).map_err(|e| GrantError::server(&e.to_string()))?;
    if !requested.0.is_empty() && !requested.is_subset_of(&app_scopes) {
        return Err(invalid_scope(
            "The requested scope is outside the application's scopes.",
        ));
    }
    let device_code = random_secret();
    let user_code = format!("{}-{}", short_code(), short_code());
    let scope = if requested.0.is_empty() {
        app.scopes.clone()
    } else {
        requested.as_string()
    };
    let entry = DeviceCodeEntry {
        application_id: app.id,
        scope,
        user_code: user_code.clone(),
        status: "pending".to_string(),
        user_id: None,
        granted_scope: None,
    };
    set_secret_key(
        state,
        device_domain(),
        &device_code,
        &json!(entry),
        DEVICE_CODE_TTL_SECONDS,
    )
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;

    // user_code → device_code index (same TTL).
    let mut conn = state
        .redis
        .conn()
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    let domain = user_code_domain();
    let index_key = state.redis.key(&format!("{domain}:{user_code}"));
    conn.set_ex::<_, _, ()>(index_key, device_code.clone(), DEVICE_CODE_TTL_SECONDS)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;

    let base = crate::activitypub::federation_base_url(&state.config);
    Ok(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": format!("{base}/oauth/device"),
        "expires_in": DEVICE_CODE_TTL_SECONDS,
        "interval": DEVICE_POLL_INTERVAL_SECONDS,
    }))
}

/// `POST /oauth/device/authorize` — an authenticated user approves a
/// `user_code`, optionally narrowing the granted scopes.
pub async fn approve_device_code(
    state: &AppState,
    user_id: i64,
    user_code: &str,
    scope: Option<&str>,
) -> Result<bool> {
    let normalized = user_code.trim().to_uppercase();
    let mut conn = state.redis.conn().await?;
    let domain = user_code_domain();
    let index_key = state.redis.key(&format!("{domain}:{normalized}"));
    let device_code: Option<String> = conn.get(&index_key).await?;
    let Some(device_code) = device_code else {
        return Ok(false);
    };
    let mut entry: DeviceCodeEntry =
        match get_secret_key(state, device_domain(), &device_code).await? {
            Some(e) => e,
            None => return Ok(false),
        };
    if entry.status != "pending" {
        return Ok(false);
    }
    let granted_scope = match scope {
        Some(raw) if !raw.trim().is_empty() => {
            let requested = ScopeSet::parse(raw)?;
            let device_scopes = ScopeSet::parse(&entry.scope)?;
            if !requested.is_subset_of(&device_scopes) {
                return Err(Error::Validation(
                    "approved scopes must be a subset of the requested scopes".into(),
                ));
            }
            requested.as_string()
        }
        _ => entry.scope.clone(),
    };
    entry.status = "granted".to_string();
    entry.user_id = Some(user_id);
    entry.granted_scope = Some(granted_scope);
    // Rewrite with the key's remaining TTL so approval doesn't extend it.
    let domain = device_domain();
    let key = state.redis.key(&format!(
        "{domain}:{}",
        crate::http::middleware::auth::token_hash(&device_code)
    ));
    let encoded = serde_json::to_string(&entry)
        .map_err(|e| Error::Internal(format!("serialize device entry: {e}")))?;
    set_keep_ttl(&mut conn, &key, &encoded).await?;
    Ok(true)
}

/// `SET KEEPTTL` needs Redis 6.2 — this Lua equivalent works on all
/// supported versions.
async fn set_keep_ttl(
    conn: &mut bb8::PooledConnection<'_, bb8_redis::RedisConnectionManager>,
    key: &str,
    value: &str,
) -> Result<()> {
    static SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
        redis::Script::new(
            "local ttl = redis.call('PTTL', KEYS[1])\n\
             if ttl < 0 then ttl = 1 end\n\
             redis.call('SET', KEYS[1], ARGV[1], 'PX', ttl)\n\
             return 1",
        )
    });
    SCRIPT
        .clone()
        .key(key)
        .arg(value)
        .invoke_async::<()>(&mut **conn)
        .await?;
    Ok(())
}

/// Device-code grant polling. `Ok(entry)` means granted and consumed —
/// the caller issues the token pair.
pub async fn poll_device_code(
    state: &AppState,
    app: &AppRow,
    device_code: &str,
) -> std::result::Result<DeviceCodeEntry, GrantError> {
    let entry: Option<DeviceCodeEntry> = get_secret_key(state, device_domain(), device_code)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    let Some(entry) = entry else {
        return Err(expired_token());
    };
    if entry.application_id != app.id {
        return Err(invalid_client("Client authentication failed."));
    }
    let hash = crate::http::middleware::auth::token_hash(device_code);
    let mut conn = state
        .redis
        .conn()
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    let poll_key = state.redis.key(&format!("{}:{hash}", device_poll_domain()));
    if entry.status == "pending" {
        // The poll gate is an atomic `SET NX EX` marker — concurrent
        // polls can't both slip through a read-modify-write window.
        let acquired: Option<String> = redis::cmd("SET")
            .arg(&poll_key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(DEVICE_POLL_INTERVAL_SECONDS)
            .query_async(&mut *conn)
            .await
            .map_err(|e| GrantError::server(&e.to_string()))?;
        return Err(if acquired.is_some() {
            authorization_pending()
        } else {
            slow_down()
        });
    }
    // Granted — consume atomically so concurrent polls can't both win,
    // then drop the user_code index and poll marker (best effort; both
    // expire with the TTL anyway).
    let consumed: Option<DeviceCodeEntry> = take_secret_key(state, device_domain(), device_code)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    let Some(entry) = consumed else {
        return Err(expired_token());
    };
    let index_key = state
        .redis
        .key(&format!("{}:{}", user_code_domain(), entry.user_code));
    if let Err(e) = conn.del::<_, i64>(vec![index_key, poll_key]).await {
        tracing::warn!(error = %e, "device-code cleanup failed");
    }
    Ok(entry)
}

// ---------------------------------------------------------------------------
// Authorized applications
// ---------------------------------------------------------------------------

/// `GET /api/v1/oauth/authorized_applications` — the user's grants,
/// joined to their applications.
pub async fn list_authorized_applications(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT a.id, a.scopes, a.created_at, \
                ap.name, ap.website, ap.redirect_uris, ap.client_id \
         FROM oauth_authorizations a \
         JOIN oauth_applications ap ON ap.id = a.application_id \
         WHERE a.user_id = $1 ORDER BY a.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let uris: Vec<String> = r.get("redirect_uris");
            json!({
                "id": r.get::<i64, _>("id").to_string(),
                "name": r.get::<String, _>("name"),
                "website": r.get::<Option<String>, _>("website"),
                "scopes": r.get::<String, _>("scopes").split_whitespace().collect::<Vec<_>>(),
                "redirect_uri": uris.first().cloned().unwrap_or_default(),
                "client_id": r.get::<String, _>("client_id"),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect())
}

/// `DELETE /api/v1/oauth/authorized_applications/{id}` — delete the
/// authorization and revoke every token minted under it.
pub async fn revoke_authorization(
    pool: &sqlx::PgPool,
    user_id: i64,
    authorization_id: i64,
) -> Result<bool> {
    let application_id: Option<i64> = sqlx::query_scalar(
        "SELECT application_id FROM oauth_authorizations WHERE id = $1 AND user_id = $2",
    )
    .bind(authorization_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(application_id) = application_id else {
        return Ok(false);
    };
    crate::auth::tokens::revoke_app_tokens(pool, user_id, application_id).await?;
    Ok(true)
}

/// 4-char uppercase alphanumeric group for `user_code`s (`XXXX-XXXX`).
fn short_code() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = OsRng;
    (0..4)
        .map(|_| {
            let index = (rng.next_u32() as usize) % ALPHABET.len();
            ALPHABET[index] as char
        })
        .collect()
}
