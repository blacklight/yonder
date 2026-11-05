//! Remote OAuth + API import (epic 31 §9): authorization-code flow
//! against a remote Mastodon-compatible instance, encrypted
//! `oauth_connections` credentials, and the `source_type = 'api'`
//! import driver — category fetchers with per-category cursors,
//! caps, rate-limit handling and resumable batches.

use super::{crypto, import::*, restore};
use crate::activitypub::ApContext;
use crate::{
    config::Config,
    http::AppState,
    jobs::{JobContext, JobKind},
    users::relationships::{self, FollowOpts},
    Error, Result,
};
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use serde_json::{json, Value};
use sqlx::Row;
use std::{collections::HashMap, time::Duration};

/// Pending-authorization TTL and Redis key namespace.
const PENDING_TTL_SECS: u64 = 900;
const PENDING_PREFIX: &str = "import-oauth:";
/// Server-side cap for `options.limit` (primary activities).
const MAX_PRIMARY: i64 = 5_000;
/// Statuses/relations fetched per page (provider-side bound).
const PAGE: usize = 40;
/// Pages fetched per batch run — keeps a run inside `BATCH_SECONDS`.
const PAGES_PER_RUN: usize = 8;
/// Cap on received-context expansion per session.
const MAX_OBSERVED_STATUSES: usize = 200;
/// Cap on remote actor resolutions (webfinger) per session.
const MAX_ACTOR_RESOLUTIONS: usize = 200;
/// Largest response body read into memory for one provider call.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// Pages followed per observation endpoint for one subject status.
const OBSERVATION_PAGES: usize = 10;
/// Consecutive malformed 2xx responses for one cursor before its
/// category is recorded as failed, and the delay between attempts.
const MAX_MALFORMED_TRIES: i64 = 3;
const MALFORMED_RETRY: Duration = Duration::from_secs(60);

/// The selectable API-import categories.
pub const CATEGORIES: &[&str] = &[
    "statuses",
    "dms",
    "bookmarks",
    "favourites",
    "following",
    "blocks",
    "mutes",
    "profile",
    "observations",
];

// ---------------------------------------------------------------------------
// Provider client — origin-locked, SSRF-guarded, bearer JSON
// ---------------------------------------------------------------------------

/// What the provider answered that matters to the driver.
pub struct RemotePage {
    pub status: u16,
    pub items: Vec<Value>,
    /// `Link: <…>; rel="next"` target — same-origin enforced.
    pub next: Option<String>,
    pub retry_after: Option<Duration>,
    /// A capability endpoint answered 404 — mark it unsupported.
    pub unsupported: bool,
}

/// Errors the driver must distinguish (contract §9.3).
#[derive(Debug)]
pub enum RemoteError {
    /// 401/403 — pause for reauthorization, never endless retry.
    Auth,
    /// 429/5xx/network — durable delayed retry.
    Transient(Option<Duration>, String),
    /// Malformed payload/URL — logged against the item, not fatal.
    Validation(String),
    /// A 2xx body that is not the expected JSON shape (a proxy page,
    /// a truncated document) — retried at the same cursor before the
    /// category gives up.
    Malformed(String),
}

/// A guarded client pinned to one origin: every request URL —
/// including pagination links — must stay inside `origin`, so bearer
/// material can never cross origins (there are no redirects either).
#[derive(Clone)]
pub struct Provider {
    origin: String,
    token: String,
    agent: ureq::Agent,
    allow_private: bool,
}

impl Provider {
    /// Build a client for `origin` + bearer `token`. `https` is
    /// required unless `federation.allow_private_targets` (dev/test).
    pub fn new(origin: &str, token: &str, config: &Config) -> Result<Self> {
        let origin = normalize_origin(origin, config)?;
        // Bounds one request; a run makes at most `PAGES_PER_RUN` of them
        // past the deadline check, so a single run stays far below the
        // sweep's 30-minute stale threshold at any sane timeout.
        let timeout = Duration::from_secs(config.federation.request_timeout_seconds.max(15));
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        Ok(Provider {
            origin,
            token: token.to_string(),
            agent,
            allow_private: config.federation.allow_private_targets,
        })
    }

    /// The origin's host — used to qualify source-local `acct`s.
    fn origin_host(&self) -> String {
        url::Url::parse(&self.origin)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default()
    }

    /// Same-origin check for absolute URLs (pagination targets).
    fn check_url(&self, url: &str) -> Result<()> {
        let ok = url::Url::parse(url)
            .map(|u| {
                matches!(u.scheme(), "http" | "https")
                    && url.starts_with(&format!("{}/", self.origin.trim_end_matches('/')))
            })
            .unwrap_or(false);
        if !ok {
            return Err(Error::Validation(format!(
                "refusing off-origin URL {url:?}"
            )));
        }
        if !self.allow_private && !pubby_sdk::http::is_safe_outbound_url(url) {
            return Err(Error::Validation(format!(
                "refusing unsafe outbound URL {url:?}"
            )));
        }
        Ok(())
    }

    fn url(&self, path: &str) -> Result<String> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.origin, path)
        };
        self.check_url(&url)?;
        Ok(url)
    }

    /// GET a JSON list page: items, next-page link, rate-limit
    /// signals. The body must be an array — an object-shaped error
    /// returned with a 200 is a validation failure, never a
    /// one-item page that would look like end-of-collection.
    pub async fn get_list(&self, path: &str) -> std::result::Result<RemotePage, RemoteError> {
        self.get_shaped(path, Shape::List).await
    }

    /// GET a JSON document (account, WebFinger, context). The body
    /// must be an object — an array is a wrong-shape response.
    pub async fn get_doc(&self, path: &str) -> std::result::Result<RemotePage, RemoteError> {
        self.get_shaped(path, Shape::Doc).await
    }

    async fn get_shaped(
        &self,
        path: &str,
        expect: Shape,
    ) -> std::result::Result<RemotePage, RemoteError> {
        let url = self
            .url(path)
            .map_err(|e| RemoteError::Validation(e.to_string()))?;
        let agent = self.agent.clone();
        let token = format!("Bearer {}", self.token);
        let page = blocking(move || {
            agent
                .get(&url)
                .header("authorization", &token)
                .header("accept", "application/json")
                .call()
                .map_err(transport)
                .and_then(|r| read_response(r, expect))
        })
        .await?;
        if page.unsupported || (200..300).contains(&page.status) {
            return Ok(page);
        }
        Err(classify(&page))
    }

    /// POST a JSON document (app registration, token exchange,
    /// revoke). No bearer header: these calls run before a token
    /// exists or authenticate with the client credentials in the body.
    pub async fn post_public(
        &self,
        path: &str,
        body: &Value,
    ) -> std::result::Result<Value, RemoteError> {
        let url = self
            .url(path)
            .map_err(|e| RemoteError::Validation(e.to_string()))?;
        let agent = self.agent.clone();
        let body_bytes = serde_json::to_vec(body).unwrap_or_default();
        let page = blocking(move || {
            agent
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .send(&body_bytes[..])
                .map_err(transport)
                .and_then(|r| read_response(r, Shape::Doc))
        })
        .await?;
        if !(200..300).contains(&page.status) {
            return Err(classify(&page));
        }
        Ok(page.items.into_iter().next().unwrap_or(json!({})))
    }
}

/// Run blocking HTTP work on the blocking pool; a join failure is a
/// transient worker problem, never a page.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::result::Result<T, RemoteError> + Send + 'static,
) -> std::result::Result<T, RemoteError> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(RemoteError::Transient(None, format!("join: {e}"))))
}

fn transport(e: ureq::Error) -> RemoteError {
    RemoteError::Transient(None, format!("request: {e}"))
}

/// The top-level JSON shape a caller expects.
#[derive(Clone, Copy)]
enum Shape {
    /// List endpoints — the body must be an array.
    List,
    /// Document endpoints — the body must be an object.
    Doc,
}

/// A failed 2xx body read: the size cap is a deterministic refusal
/// (validation), while an interrupted read is transport trouble that
/// must retry with the cursor unchanged — treating it as validation
/// would permanently skip the rest of a category.
fn body_read_error(e: ureq::Error) -> RemoteError {
    let oversized = match &e {
        ureq::Error::BodyExceedsLimit(_) => true,
        ureq::Error::Io(ioe) => ioe
            .get_ref()
            .and_then(|r| r.downcast_ref::<ureq::Error>())
            .is_some_and(|u| matches!(u, ureq::Error::BodyExceedsLimit(_))),
        _ => false,
    };
    if oversized {
        RemoteError::Validation(format!("response body exceeds {MAX_BODY_BYTES} bytes"))
    } else {
        RemoteError::Transient(None, format!("response body unreadable: {e}"))
    }
}

/// Read headers and (for 2xx) the body. Runs on the blocking pool
/// together with the request, so no socket I/O touches the async worker.
fn read_response(
    mut resp: ureq::http::Response<ureq::Body>,
    expect: Shape,
) -> std::result::Result<RemotePage, RemoteError> {
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after);
    let next = resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_link_next);
    let body = if (200..300).contains(&status) {
        resp.body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_vec()
            .map_err(body_read_error)?
    } else {
        Vec::new()
    };
    parse_page(status, retry_after, next, &body, expect)
}

/// Status → page. A 2xx body must be JSON of the expected shape;
/// anything else is a validation error so a broken success never
/// reads as an empty list. Non-2xx bodies are ignored here and
/// classified by [`classify`].
fn parse_page(
    status: u16,
    retry_after: Option<Duration>,
    next: Option<String>,
    body: &[u8],
    expect: Shape,
) -> std::result::Result<RemotePage, RemoteError> {
    let items = if (200..300).contains(&status) {
        page_items(body, expect).map_err(RemoteError::Malformed)?
    } else {
        Vec::new()
    };
    Ok(RemotePage {
        status,
        items,
        next,
        retry_after,
        unsupported: status == 404,
    })
}

fn classify(page: &RemotePage) -> RemoteError {
    match page.status {
        401 | 403 => RemoteError::Auth,
        404 => RemoteError::Validation("endpoint not found".into()),
        429 => RemoteError::Transient(page.retry_after, "rate limited".into()),
        s if s >= 500 => RemoteError::Transient(page.retry_after, format!("HTTP {s}")),
        s => RemoteError::Validation(format!("HTTP {s}")),
    }
}

/// Parsed-body guard: 2xx responses whose body is not JSON (a proxy
/// HTML page, a truncated body) or not the expected top-level shape
/// are validation failures, not a silent empty page that would look
/// like end-of-collection.
fn page_items(body: &[u8], expect: Shape) -> std::result::Result<Vec<Value>, String> {
    match serde_json::from_slice::<Value>(body).map_err(|e| format!("malformed JSON body: {e}"))? {
        Value::Array(a) if matches!(expect, Shape::List) => Ok(a),
        v @ Value::Object(_) if matches!(expect, Shape::Doc) => Ok(vec![v]),
        _ => Err("unexpected JSON shape in response".into()),
    }
}

/// `Retry-After` — seconds or an HTTP date.
fn parse_retry_after(v: &str) -> Option<Duration> {
    if let Ok(secs) = v.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs.min(3600)));
    }
    DateTime::parse_from_rfc2822(v).ok().map(|t| {
        (t.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or_default()
            .min(Duration::from_secs(3600))
    })
}

/// First `<url>; rel="next"` in a Link header.
fn parse_link_next(v: &str) -> Option<String> {
    for part in v.split(',') {
        if part.contains("rel=\"next\"") {
            if let (Some(a), Some(b)) = (part.find('<'), part.find('>')) {
                return Some(part[a + 1..b].to_string());
            }
        }
    }
    None
}

/// Normalize an origin: `https` (or `http` under
/// `allow_private_targets`), no path/query, SSRF-guarded. A bare
/// domain (`mastodon.example`) or an acct handle (`@you@mastodon.example`)
/// is accepted too — only the host is an origin, so it becomes
/// `https://{host}` before the usual checks.
fn normalize_origin(origin: &str, config: &Config) -> Result<String> {
    let raw = origin.trim();
    let raw = if raw.contains("://") {
        raw.to_string()
    } else {
        // Take the host of an acct handle (`user@host`, `@user@host`,
        // `@host`) or the bare input as a domain.
        format!("https://{}", raw.rsplit('@').next().unwrap_or_default())
    };
    let u = url::Url::parse(&raw).map_err(|_| {
        Error::Validation("origin must be an https URL, domain or @user@host".into())
    })?;
    let scheme_ok =
        u.scheme() == "https" || (u.scheme() == "http" && config.federation.allow_private_targets);
    let bare = (u.path() == "/" || u.path().is_empty())
        && u.query().is_none()
        && u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none();
    if !scheme_ok || !bare {
        return Err(Error::Validation(
            "origin must be an https URL without a path, query or credentials".into(),
        ));
    }
    let origin = format!("{}://{}", u.scheme(), u.host_str().unwrap_or_default());
    let origin = match u.port() {
        Some(p) => format!("{origin}:{p}"),
        None => origin,
    };
    if !config.federation.allow_private_targets && !pubby_sdk::http::is_safe_outbound_url(&origin) {
        return Err(Error::Validation("origin fails the SSRF guard".into()));
    }
    Ok(origin)
}

// ---------------------------------------------------------------------------
// OAuth connections
// ---------------------------------------------------------------------------

fn pending_key(state: &str) -> String {
    format!("{PENDING_PREFIX}{state}")
}

fn callback_uri(state: &AppState) -> String {
    format!(
        "{}/api/v1/yonder/import_connections/callback",
        crate::activitypub::federation_base_url(state.config().as_ref())
    )
}

/// Cookie that binds the callback to the browser that began the flow.
pub const BINDING_COOKIE: &str = "yonder_import_oauth";

/// `Set-Cookie` value carrying the per-flow binding token. Scoped to
/// the connections path so it never travels with other requests.
pub fn binding_cookie(callback: &str, binding: &str) -> String {
    binding_cookie_with_age(callback, binding, PENDING_TTL_SECS)
}

/// `Set-Cookie` value that clears the binding cookie once the callback
/// has consumed its flow, so the token does not outlive the attempt.
pub fn expired_binding_cookie(callback: &str) -> String {
    binding_cookie_with_age(callback, "", 0)
}

fn binding_cookie_with_age(callback: &str, value: &str, max_age: u64) -> String {
    let secure = if callback.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{BINDING_COOKIE}={value}; Path=/api/v1/yonder/import_connections; \
         Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    )
}

/// The binding token from a request's `Cookie` header, if present.
pub fn binding_from_cookie(header: Option<&str>) -> Option<String> {
    header?
        .split(';')
        .filter_map(|c| c.trim().strip_prefix(BINDING_COOKIE)?.strip_prefix('='))
        .map(String::from)
        .find(|v| !v.is_empty())
}

/// A `return_to` redirect path is local-only: it must start with a
/// single `/` (never `//` — protocol-relative URLs leave the site, and
/// never `/\` because browsers fold `\` into `/`), and contain only
/// printable ASCII with no `\`, `?` or `#` (the callback appends its
/// own query). Anything else is rejected, so the value stored in the
/// pending blob can be trusted verbatim at callback time.
pub(crate) fn validate_return_to(path: &str) -> Result<&str> {
    let local = path.starts_with('/')
        && path[1..]
            .chars()
            .next()
            .is_some_and(|c| c != '/' && c != '\\')
        && path
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_control() && !matches!(c, '\\' | '?' | '#'));
    if local {
        Ok(path)
    } else {
        Err(Error::Validation("return_to must be a local path".into()))
    }
}

/// `POST /api/v1/yonder/import_connections` — register the remote app
/// and return the authorization URL plus the browser-binding token.
pub async fn begin_authorization(
    state: &AppState,
    user_id: i64,
    origin: &str,
    categories: &[String],
    return_to: Option<&str>,
) -> Result<(String, String)> {
    if !crypto::available(state.config().as_ref()) {
        return Err(Error::Internal(
            "remote import is unavailable: auth.secret_key is not configured".into(),
        ));
    }
    let origin = normalize_origin(origin, state.config().as_ref())?;
    let return_to = return_to.map(validate_return_to).transpose()?;
    let scopes = requested_scopes(categories);
    // App registration is unauthenticated on Mastodon-compatible
    // servers — a throwaway provider with an empty token is fine.
    let client = Provider::new(&origin, "", state.config().as_ref())?;
    let app = client
        .post_public(
            "/api/v1/apps",
            &json!({
                "client_name": "Yonder",
                "redirect_uris": callback_uri(state),
                "scopes": scopes,
            }),
        )
        .await
        .map_err(|e| match e {
            RemoteError::Auth => Error::Forbidden("the remote instance refused app registration"),
            RemoteError::Transient(_, m) => {
                Error::Internal(format!("remote app registration failed: {m}"))
            }
            RemoteError::Validation(m) | RemoteError::Malformed(m) => Error::Validation(m),
        })?;
    let client_id = app
        .get("client_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let client_secret = app
        .get("client_secret")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err(Error::Validation(
            "remote app registration returned no credentials".into(),
        ));
    }
    let (verifier, challenge) = crypto::pkce_pair();
    let nonce = crypto::rand_token();
    let binding = crypto::rand_token();
    let pending = json!({
        "user_id": user_id,
        "origin": origin,
        "client_id": client_id,
        "client_secret_enc": crypto::encrypt(state.config().as_ref(), client_secret.as_bytes())?,
        "verifier_enc": crypto::encrypt(state.config().as_ref(), verifier.as_bytes())?,
        "scopes": scopes,
        "binding": binding,
        "return_to": return_to,
    });
    let blob = crypto::encrypt(state.config().as_ref(), pending.to_string().as_bytes())?;
    let mut conn = state.redis.conn().await?;
    let _: () = conn
        .set_ex(pending_key(&nonce), base64_encode(&blob), PENDING_TTL_SECS)
        .await?;
    let url = format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        origin,
        urlenc(&client_id),
        urlenc(&callback_uri(state)),
        urlenc(&scopes),
        nonce,
        challenge,
    );
    Ok((url, binding))
}

/// `GET …/callback?code&state` — single-use state, browser binding
/// (cookie must match the token stored with the state), code exchange,
/// identity verification, encrypted credential persistence. The
/// pending state's `return_to` comes back with the outcome, so the
/// callback can send a failed flow back to the page that started it.
pub async fn complete_authorization(
    state: &AppState,
    code: &str,
    nonce: &str,
    binding: Option<&str>,
) -> (Result<i64>, Option<String>) {
    let mut return_to = None;
    let result = finish_authorization(state, code, nonce, binding, &mut return_to).await;
    (result, return_to)
}

async fn finish_authorization(
    state: &AppState,
    code: &str,
    nonce: &str,
    binding: Option<&str>,
    return_to: &mut Option<String>,
) -> Result<i64> {
    let mut conn = state.redis.conn().await?;
    let blob_b64: Option<String> = conn.get(pending_key(nonce)).await?;
    let Some(blob_b64) = blob_b64 else {
        return Err(Error::Validation(
            "unknown or expired authorization state".into(),
        ));
    };
    let pending: Value = serde_json::from_slice(&crypto::decrypt(
        state.config().as_ref(),
        &base64_decode(&blob_b64).ok_or_else(|| Error::Validation("bad pending state".into()))?,
    )?)
    .map_err(|_| Error::Internal("pending state unreadable".into()))?;
    // A callback from a different browser (a link the victim was sent
    // from an attacker-started flow) must not bind the consenting
    // account to the initiator. The state is only consumed once the
    // browser is proven, so a mismatched callback leaves it intact.
    if binding.is_none() || pending.get("binding").and_then(Value::as_str) != binding {
        return Err(Error::Forbidden(
            "authorization must be completed in the browser that started it",
        ));
    }
    let consumed: Option<String> = redis::cmd("GETDEL")
        .arg(pending_key(nonce))
        .query_async(&mut *conn)
        .await?;
    if consumed.is_none() {
        return Err(Error::Validation(
            "unknown or expired authorization state".into(),
        ));
    }
    *return_to = pending
        .get("return_to")
        .and_then(Value::as_str)
        .map(String::from);
    let user_id: i64 = pending
        .get("user_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::Internal("pending state has no user".into()))?;
    let origin = pending
        .get("origin")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let client_id = pending
        .get("client_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let client_secret = crypto::decrypt(
        state.config().as_ref(),
        &bytes_of(pending.get("client_secret_enc"))?,
    )?;
    let verifier = crypto::decrypt(
        state.config().as_ref(),
        &bytes_of(pending.get("verifier_enc"))?,
    )?;
    let scopes = pending
        .get("scopes")
        .and_then(Value::as_str)
        .unwrap_or("read");

    let client = Provider::new(&origin, "", state.config().as_ref())?;
    let token = client
        .post_public(
            "/oauth/token",
            &json!({
                "grant_type": "authorization_code",
                "code": code,
                "client_id": client_id,
                "client_secret": String::from_utf8_lossy(&client_secret),
                "redirect_uri": callback_uri(state),
                "code_verifier": String::from_utf8_lossy(&verifier),
                "scope": scopes,
            }),
        )
        .await
        .map_err(|e| match e {
            RemoteError::Auth => Error::Forbidden("the remote instance refused the code exchange"),
            RemoteError::Transient(_, m) => Error::Internal(format!("token exchange failed: {m}")),
            RemoteError::Validation(m) | RemoteError::Malformed(m) => Error::Validation(m),
        })?;
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if access.is_empty() {
        return Err(Error::Validation(
            "token exchange returned no access token".into(),
        ));
    }
    let refresh = token.get("refresh_token").and_then(Value::as_str);
    let granted_scopes = token
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or(scopes)
        .to_string();

    // Identity: verify_credentials → the canonical actor via webfinger.
    let client = Provider::new(&origin, &access, state.config().as_ref())?;
    let creds = get_json(&client, "/api/v1/accounts/verify_credentials").await?;
    let Some((_id, acct)) = account_fields(&creds) else {
        return Err(Error::Validation(
            "verify_credentials returned no usable account".into(),
        ));
    };
    // Fail closed: a profile page URL is not the canonical actor, and
    // a connection stored under it could never be verified later.
    let actor_uri = actor_uri_for(&client, &acct)
        .await
        .map_err(|e| match e {
            RemoteError::Auth => Error::Forbidden("the remote instance refused identity lookup"),
            RemoteError::Transient(_, m) => Error::Internal(format!("identity lookup failed: {m}")),
            RemoteError::Validation(m) | RemoteError::Malformed(m) => Error::Validation(m),
        })?
        .ok_or_else(|| {
            Error::Internal(
                "could not verify the remote account's canonical actor; try again".into(),
            )
        })?;
    let source_acct = if acct.contains('@') {
        acct.clone()
    } else {
        format!(
            "{acct}@{}",
            url::Url::parse(&origin)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
                .unwrap_or_default()
        )
    };

    let conn_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_connections \
           (user_id, origin, source_actor_uri, source_acct, scopes, \
            access_token_enc, refresh_token_enc, client_id_enc, client_secret_enc, \
            state) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'active') \
         ON CONFLICT (user_id, origin, source_actor_uri) DO UPDATE SET \
           scopes = EXCLUDED.scopes, access_token_enc = EXCLUDED.access_token_enc, \
           refresh_token_enc = EXCLUDED.refresh_token_enc, \
           client_id_enc = EXCLUDED.client_id_enc, \
           client_secret_enc = EXCLUDED.client_secret_enc, \
           source_acct = EXCLUDED.source_acct, \
           state = 'active', revoked_at = NULL, updated_at = now() \
         RETURNING id",
    )
    .bind(user_id)
    .bind(&origin)
    .bind(&actor_uri)
    .bind(&source_acct)
    .bind(&granted_scopes)
    .bind(crypto::encrypt(state.config().as_ref(), access.as_bytes())?)
    .bind(crypto::encrypt_opt(state.config().as_ref(), refresh)?)
    .bind(crypto::encrypt(
        state.config().as_ref(),
        client_id.as_bytes(),
    )?)
    .bind(crypto::encrypt(
        state.config().as_ref(),
        client_secret.as_slice(),
    )?)
    .fetch_one(&state.db)
    .await?;
    Ok(conn_id)
}

/// The minimal read-only scope set for the selected categories.
fn requested_scopes(categories: &[String]) -> String {
    let mut scopes: Vec<&str> = vec!["read:accounts"];
    for c in categories {
        match c.as_str() {
            "following" => scopes.push("read:follows"),
            "blocks" => scopes.push("read:blocks"),
            "mutes" => scopes.push("read:mutes"),
            "bookmarks" => scopes.push("read:bookmarks"),
            "favourites" => scopes.push("read:favourites"),
            "dms" | "statuses" | "observations" | "profile" => scopes.push("read"),
            _ => {}
        }
    }
    if scopes.contains(&"read") {
        return "read".into();
    }
    scopes.sort();
    scopes.dedup();
    scopes.join(" ")
}

/// The scopes that can serve a category — the umbrella `read` or a
/// provider's granular equivalent (`read:statuses` covers the
/// conversations walk as well as the statuses one).
fn required_scopes(cat: &str) -> &'static [&'static str] {
    match cat {
        "following" => &["read", "read:follows", "read:accounts"],
        "blocks" => &["read", "read:blocks"],
        "mutes" => &["read", "read:mutes"],
        "bookmarks" => &["read", "read:bookmarks"],
        "favourites" => &["read", "read:favourites"],
        "dms" | "statuses" | "observations" => &["read", "read:statuses"],
        "profile" => &["read", "read:accounts"],
        _ => &["read"],
    }
}

/// Categories a connection's granted scopes cannot serve. A session
/// bound to an under-scoped connection can only pause mid-run once
/// the provider 403s the first gated endpoint — surface that at
/// bind time instead.
fn scope_uncovered(granted: &str, categories: &[String]) -> Vec<String> {
    let granted: std::collections::HashSet<&str> = granted.split_whitespace().collect();
    categories
        .iter()
        .filter(|c| !required_scopes(c).iter().any(|s| granted.contains(s)))
        .cloned()
        .collect()
}

fn assert_scope_coverage(granted: &str, categories: &[String]) -> Result<()> {
    let missing = scope_uncovered(granted, categories);
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "the connection's granted scopes ({granted}) do not cover: {} — \
         reconnect granting those categories and retry",
        missing.join(", ")
    )))
}

/// `GET /api/v1/yonder/import_connections` — projections, never secrets.
pub async fn list_connections(state: &AppState, user_id: i64) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT id, origin, source_actor_uri, source_acct, scopes, state, created_at \
         FROM oauth_connections WHERE user_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!(rows
        .iter()
        .map(|r| json!({
            "id": r.get::<i64, _>("id").to_string(),
            "origin": r.get::<String, _>("origin"),
            "source_actor_uri": r.get::<String, _>("source_actor_uri"),
            "source_acct": r.get::<Option<String>, _>("source_acct"),
            "scopes": r.get::<String, _>("scopes"),
            "state": r.get::<String, _>("state"),
            "created_at": r.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
        }))
        .collect::<Vec<_>>()))
}

/// `DELETE …/{id}` — remote revocation (best effort) + tombstone.
pub async fn revoke_connection(state: &AppState, user_id: i64, id: i64) -> Result<()> {
    let row = sqlx::query(
        "SELECT origin, client_id_enc, client_secret_enc, access_token_enc \
         FROM oauth_connections WHERE id = $1 AND user_id = $2 AND state = 'active'",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("connection".into()))?;
    let origin: String = row.get("origin");
    // Remote revoke is strictly best-effort: unreadable ciphertext
    // (key rotation, corruption) or a dead provider must never block
    // the local tombstone — the alternative is an unrevokable
    // connection.
    let dec = |col: &str| {
        crypto::decrypt(state.config().as_ref(), &row.get::<Vec<u8>, _>(col))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).to_string())
    };
    if let (Some(client_id), Some(secret), Some(access)) = (
        dec("client_id_enc"),
        dec("client_secret_enc"),
        dec("access_token_enc"),
    ) {
        if let Ok(client) = Provider::new(&origin, "", state.config().as_ref()) {
            let _ = client
                .post_public(
                    "/oauth/revoke",
                    &json!({
                        "client_id": client_id,
                        "client_secret": secret,
                        "token": access,
                    }),
                )
                .await;
        }
    }
    // Secrets die with the grant unless a live session still needs
    // the connection — a paused session's resume re-verifies and
    // pauses for reauthorization rather than minting requests from a
    // revoked grant. The in-use check and the erasure are one
    // statement, so they cannot disagree with a concurrent resume.
    sqlx::query(
        "UPDATE oauth_connections c SET state = 'revoked', revoked_at = now(), updated_at = now(), \
                access_token_enc = CASE WHEN s.in_use THEN c.access_token_enc ELSE '\\x'::bytea END, \
                refresh_token_enc = CASE WHEN s.in_use THEN c.refresh_token_enc ELSE NULL END, \
                client_id_enc = CASE WHEN s.in_use THEN c.client_id_enc ELSE '\\x'::bytea END, \
                client_secret_enc = CASE WHEN s.in_use THEN c.client_secret_enc ELSE '\\x'::bytea END \
         FROM (SELECT EXISTS(SELECT 1 FROM archive_imports i \
                WHERE i.options->>'connection_id' = $3 \
                  AND i.state IN ('pending','running','paused')) AS in_use) s \
         WHERE c.id = $1 AND c.user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    // Text comparison — `->>` already yields text, and a cast would
    // fail the whole revoke on a non-numeric stored value.
    .bind(id.to_string())
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn get_json(client: &Provider, path: &str) -> Result<Value> {
    let page = client.get_doc(path).await.map_err(provider_error)?;
    if page.unsupported {
        return Err(Error::NotFound("remote endpoint".into()));
    }
    page.items
        .into_iter()
        .next()
        .filter(Value::is_object)
        .ok_or_else(|| Error::Validation(format!("remote {path} returned no object")))
}

/// Typed identity from an account-shaped response: `id` and `acct`
/// must both be present and nonempty — a `{"error": "…"}` shaped
/// success is not an account.
fn account_fields(v: &Value) -> Option<(String, String)> {
    let id = v.get("id").and_then(|i| {
        i.as_str()
            .map(String::from)
            .or_else(|| i.as_i64().map(|n| n.to_string()))
    })?;
    let acct = v.get("acct").and_then(Value::as_str)?;
    if id.is_empty() || acct.is_empty() {
        return None;
    }
    Some((id, acct.to_string()))
}

fn provider_error(e: RemoteError) -> Error {
    match e {
        RemoteError::Auth => Error::Forbidden("remote authorization was revoked"),
        RemoteError::Transient(_, m) => Error::Internal(format!("remote request failed: {m}")),
        RemoteError::Validation(m) | RemoteError::Malformed(m) => Error::Validation(m),
    }
}

/// The validated `rel=self` link of a WebFinger doc: the returned
/// `subject` must match the queried resource and the link must be an
/// activity+json http(s) URL — a bare string from a hostile or
/// confused server is never stored as an actor identity.
fn webfinger_self(doc: &Value, resource: &str) -> Option<String> {
    if let Some(subject) = doc.get("subject").and_then(Value::as_str) {
        if subject != resource {
            return None;
        }
    }
    doc.get("links")
        .and_then(Value::as_array)?
        .iter()
        .find(|l| {
            l.get("rel").and_then(Value::as_str) == Some("self")
                && matches!(
                    l.get("type").and_then(Value::as_str),
                    Some("application/activity+json") | Some("application/ld+json")
                )
        })
        .and_then(|l| l.get("href").and_then(Value::as_str))
        .filter(|href| {
            url::Url::parse(href)
                .map(|u| matches!(u.scheme(), "http" | "https"))
                .unwrap_or(false)
        })
        .map(String::from)
}

/// actor URI for an acct via the remote's WebFinger. `Err` keeps the
/// failure class so callers can distinguish "unresolvable" from
/// "remote trouble" — only `Ok(Some)` is a verified identity.
async fn actor_uri_for(
    client: &Provider,
    acct: &str,
) -> std::result::Result<Option<String>, RemoteError> {
    if acct.is_empty() {
        return Ok(None);
    }
    let resource = if acct.contains('@') {
        format!("acct:{acct}")
    } else {
        let host = client.origin_host();
        if host.is_empty() {
            return Ok(None);
        }
        format!("acct:{acct}@{host}")
    };
    let page = client
        .get_doc(&format!(
            "/.well-known/webfinger?resource={}",
            urlenc(&resource)
        ))
        .await?;
    let Some(doc) = page.items.into_iter().next() else {
        return Ok(None);
    };
    Ok(webfinger_self(&doc, &resource))
}

fn bytes_of(v: Option<&Value>) -> Result<Vec<u8>> {
    let arr = v
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Internal("missing secret in pending state".into()))?;
    Ok(arr
        .iter()
        .filter_map(|b| b.as_u64().map(|n| n as u8))
        .collect())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

fn urlenc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ---------------------------------------------------------------------------
// Import sessions (source_type = 'api')
// ---------------------------------------------------------------------------

/// The frozen session options `create_session` persists.
pub struct SessionSpec<'a> {
    pub connection_id: i64,
    pub categories: &'a [String],
    pub limit: Option<i64>,
    pub order: &'a str,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub federate: bool,
    pub overwrite_profile: bool,
}

/// `POST /api/v1/yonder/import_sessions` — freeze options + cutoff,
/// insert the session, enqueue the shared `Import` job.
pub async fn create_session(
    state: &AppState,
    account_id: i64,
    spec: SessionSpec<'_>,
) -> Result<Value> {
    let SessionSpec {
        connection_id,
        categories,
        limit,
        order,
        since,
        until,
        federate,
        overwrite_profile,
    } = spec;
    let conn = sqlx::query(
        "SELECT id, origin, source_actor_uri, scopes FROM oauth_connections \
         WHERE id = $1 AND user_id = (SELECT u.id FROM users u JOIN accounts a \
               ON a.id = u.account_id WHERE a.id = $2) AND state = 'active'",
    )
    .bind(connection_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::Validation("connection is not usable".into()))?;
    let categories: Vec<String> = if categories.is_empty() {
        vec!["following".to_string()]
    } else {
        categories.to_vec()
    };
    for c in &categories {
        if !CATEGORIES.contains(&c.as_str()) {
            return Err(Error::Validation(format!("unknown category {c:?}")));
        }
    }
    assert_scope_coverage(&conn.get::<String, _>("scopes"), &categories)?;
    // Observation subjects are the statuses this same session imported;
    // without the statuses walk there is nothing to observe.
    if categories.iter().any(|c| c == "observations") && !categories.iter().any(|c| c == "statuses")
    {
        return Err(Error::Validation(
            "observations require the statuses category in the same session".into(),
        ));
    }
    let limit = limit.unwrap_or(MAX_PRIMARY).clamp(1, MAX_PRIMARY);
    if !matches!(order, "newest" | "oldest") {
        return Err(Error::Validation("order must be newest or oldest".into()));
    }
    if let (Some(s), Some(u)) = (since, until) {
        if u <= s {
            return Err(Error::Validation("until must be after since".into()));
        }
    }
    let options = json!({
        "connection_id": connection_id,
        "categories": categories,
        "limit": limit,
        "order": order,
        "since": since.map(|t| t.to_rfc3339()),
        "until": until.map(|t| t.to_rfc3339()),
        "federate": federate,
        "overwrite_profile": overwrite_profile,
    });
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports \
           (account_id, kind, mode, source_type, source_origin, source_actor_uri, \
            options, cutoff, stage) \
         VALUES ($1, 'api', 'merge', 'api', $2, $3, $4, now(), 'queued') RETURNING id",
    )
    .bind(account_id)
    .bind(conn.get::<String, _>("origin"))
    .bind(conn.get::<String, _>("source_actor_uri"))
    .bind(options)
    .fetch_one(&state.db)
    .await?;
    // The sweep re-enqueues stale `pending` rows — a lost envelope
    // only delays the import; an error here would invite a retry
    // that creates a duplicate session.
    if let Err(e) = state
        .dispatcher
        .enqueue(JobKind::Import, json!({ "import_id": id }))
        .await
    {
        tracing::warn!(import_id = id, error = %e, "import enqueue failed; relying on sweep");
    }
    Ok(json!({ "id": id.to_string(), "state": "pending" }))
}

/// `POST /api/v1/yonder/imports/{id}/cancel` — cooperative cancel.
pub async fn cancel_import(state: &AppState, account_id: i64, id: i64) -> Result<()> {
    // A paused session has no in-flight job to notice the flag —
    // settle it as cancelled immediately; pending/running settle on
    // the next checkpoint.
    let cancelled: Option<Option<String>> = sqlx::query_scalar(
        "UPDATE archive_imports SET cancel_requested = true, \
               state = CASE WHEN state = 'paused' THEN 'cancelled' ELSE state END, \
               completed_at = CASE WHEN state = 'paused' THEN now() ELSE completed_at END \
         WHERE id = $1 AND account_id = $2 AND state IN ('pending','running','paused') \
         RETURNING file_key",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    if cancelled.is_none() {
        return Err(Error::NotFound("import".into()));
    }
    if let Some(key) = cancelled.flatten() {
        if let Err(e) = state.media.delete(&key).await {
            tracing::warn!(import_id = id, error = %e, "cancelled-import file cleanup failed");
        }
    }
    // Pending publication work must not outlive the session: a
    // cancelled import can never legitimately fan out later (a drain
    // that never ran would otherwise find misleading pending rows).
    super::publish::fail_pending_publication(&state.db, id, "import cancelled").await?;
    Ok(())
}

/// `POST /api/v1/yonder/imports/{id}/resume` — re-enqueue a paused
/// session (optionally rebound to a fresh connection).
pub async fn resume_import(
    state: &AppState,
    account_id: i64,
    id: i64,
    connection_id: Option<i64>,
) -> Result<()> {
    // A rebind must also cover the frozen categories — resuming onto
    // an under-scoped connection just pauses again on the next gated
    // endpoint.
    if let Some(conn_id) = connection_id {
        let scopes: Option<(String, Value, Value)> = sqlx::query_as(
            "SELECT c.scopes, i.options, i.category_state FROM oauth_connections c \
             JOIN users u ON u.id = c.user_id, archive_imports i \
             WHERE c.id = $1 AND u.account_id = $2 AND i.id = $3 \
               AND i.account_id = $2 AND c.state = 'active' \
               AND c.origin = i.source_origin \
               AND c.source_actor_uri = i.source_actor_uri",
        )
        .bind(conn_id)
        .bind(account_id)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
        if let Some((granted, options, cats)) = scopes {
            // Only incomplete categories are checked — a `done` one
            // never touches the provider again.
            let categories: Vec<String> = options
                .get("categories")
                .and_then(|c| serde_json::from_value::<Vec<String>>(c.clone()).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|c| {
                    cats.get(c.as_str())
                        .and_then(|s| s.get("done"))
                        .and_then(Value::as_bool)
                        != Some(true)
                })
                .collect();
            assert_scope_coverage(&granted, &categories)?;
        }
    }
    // One owner-scoped statement: a rebind must also match the frozen
    // source (origin + verified actor) so a session can never be
    // retargeted at a different remote account — and a foreign import
    // id can never observe or mutate anything.
    let n = sqlx::query(
        "UPDATE archive_imports i SET state = 'pending', error = NULL, next_retry_at = NULL, \
               options = CASE WHEN $3::bigint IS NULL THEN i.options \
                   ELSE i.options || jsonb_build_object('connection_id', $3::bigint) END \
         WHERE i.id = $1 AND i.account_id = $2 AND i.state = 'paused' \
           AND ($3::bigint IS NULL OR EXISTS ( \
               SELECT 1 FROM oauth_connections c \
               JOIN users u ON u.id = c.user_id \
               WHERE c.id = $3 AND u.account_id = $2 AND c.state = 'active' \
                 AND c.origin = i.source_origin \
                 AND c.source_actor_uri = i.source_actor_uri))",
    )
    .bind(id)
    .bind(account_id)
    .bind(connection_id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        // Distinguish "not mine / not paused" from "rebind refused":
        // the same 404 either way — no oracle for foreign sessions.
        return Err(Error::NotFound("paused import".into()));
    }
    // A lost envelope leaves the session `pending`; the sweep
    // re-enqueues it.
    if let Err(e) = state
        .dispatcher
        .enqueue(JobKind::Import, json!({ "import_id": id }))
        .await
    {
        tracing::warn!(import_id = id, error = %e, "import resume enqueue failed; relying on sweep");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The api-source driver
// ---------------------------------------------------------------------------

struct Session {
    connection_id: i64,
    categories: Vec<String>,
    limit: i64,
    order: String,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    federate: bool,
    overwrite_profile: bool,
    source_id: String,
    /// Frozen canonical actor the session may import — verified
    /// against the connection at job start, and against the fresh
    /// `verify_credentials` identity each run.
    source_actor_uri: String,
}

fn session_of(options: &Value) -> Session {
    Session {
        connection_id: options
            .get("connection_id")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        categories: options
            .get("categories")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["following".to_string()]),
        limit: options
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(MAX_PRIMARY),
        order: options
            .get("order")
            .and_then(Value::as_str)
            .unwrap_or("newest")
            .to_string(),
        since: options
            .get("since")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc)),
        until: options
            .get("until")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc)),
        federate: options
            .get("federate")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        overwrite_profile: options
            .get("overwrite_profile")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source_id: options
            .get("source_account_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        source_actor_uri: String::new(),
    }
}

/// `source_type = 'api'` import driver: category walk with cursors.
pub(crate) async fn import_api(ctx: &JobContext, run: ImportRun<'_>) -> Result<()> {
    let ImportRun {
        ctx: _,
        rel,
        import_id,
        account_id,
        options,
        category_state,
        cutoff,
        deadline,
        lease,
        ..
    } = run;
    let mut session = session_of(&options);
    let mut cats = category_state;

    // Load + decrypt the connection; a dead connection pauses the
    // session (reauthorization → resume), never burns retries. The
    // worker re-verifies ownership and the frozen source identity —
    // the rebind endpoint already enforces it, but the job trusts
    // the row, not the request that wrote it.
    let conn_row = sqlx::query(
        "SELECT c.user_id, c.origin, c.access_token_enc, c.state, c.source_actor_uri, \
                u.account_id AS owner_account_id \
         FROM oauth_connections c JOIN users u ON u.id = c.user_id WHERE c.id = $1",
    )
    .bind(session.connection_id)
    .fetch_optional(&ctx.db)
    .await?;
    let Some(conn_row) = conn_row else {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "connection is gone — reauthorize to resume",
        )
        .await;
    };
    if conn_row.get::<String, _>("state") != "active"
        || conn_row.get::<i64, _>("owner_account_id") != account_id
    {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "connection needs reauthorization",
        )
        .await;
    }
    let origin: String = conn_row.get("origin");
    let expected: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT source_actor_uri, source_origin FROM archive_imports WHERE id = $1")
            .bind(import_id)
            .fetch_optional(&ctx.db)
            .await?;
    let (expected_actor, expected_origin) = expected.unwrap_or_default();
    // Fail closed: pause rather than walk a different account; resume
    // refuses the same mismatch.
    if expected_actor.as_deref() != Some(conn_row.get::<String, _>("source_actor_uri").as_str())
        || expected_origin.as_deref() != Some(origin.as_str())
    {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "connection does not match the session's frozen source — reauthorize the original account",
        )
        .await;
    }
    session.source_actor_uri = expected_actor.clone().unwrap_or_default();
    let Ok(token) = crypto::decrypt(&ctx.config, &conn_row.get::<Vec<u8>, _>("access_token_enc"))
    else {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "stored credentials are unreadable — reauthorize to resume",
        )
        .await;
    };
    let token = String::from_utf8_lossy(&token).to_string();
    let client = Provider::new(&origin, &token, &ctx.config)?;

    // Fresh identity check each run: the freshly-returned account —
    // not just the stored row — must resolve to the frozen actor on
    // the frozen origin. A changed/misbound credential pauses; remote
    // trouble retries; it never imports a different account.
    let creds = match get_json(&client, "/api/v1/accounts/verify_credentials").await {
        Ok(v) => v,
        Err(Error::Forbidden(m)) => {
            return pause(ctx, import_id, Some(&cats), lease, m).await;
        }
        Err(Error::Validation(m)) => {
            return pause(ctx, import_id, Some(&cats), lease, &m).await;
        }
        Err(_) => {
            return retry_later(ctx, import_id, &cats, lease, Duration::from_secs(60)).await;
        }
    };
    let Some((source_id, fresh_acct)) = account_fields(&creds) else {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "the remote did not return a usable account — reauthorize to resume",
        )
        .await;
    };
    let fresh_actor = match actor_uri_for(&client, &fresh_acct).await {
        Ok(a) => a,
        Err(RemoteError::Auth) => {
            return pause(
                ctx,
                import_id,
                Some(&cats),
                lease,
                "remote authorization was revoked — reauthorize to resume",
            )
            .await;
        }
        Err(_) => {
            return retry_later(ctx, import_id, &cats, lease, Duration::from_secs(60)).await;
        }
    };
    if fresh_actor.as_deref() != expected_actor.as_deref() {
        return pause(
            ctx,
            import_id,
            Some(&cats),
            lease,
            "the connected account no longer matches the session's source — reauthorize the original account",
        )
        .await;
    }
    session.source_id = source_id.clone();

    // The actor-resolution cache survives across batch runs inside
    // `category_state` — otherwise the per-session resolution budget
    // silently resets on every job.
    let mut actors: HashMap<String, Option<String>> = cats
        .get("_actors")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let federate = session.federate && rel.ap.is_some();
    // Category order is fixed (relationships before content,
    // statuses before observations) — never client order.
    let ordered: Vec<&str> = CATEGORIES
        .iter()
        .copied()
        .filter(|c| session.categories.iter().any(|s| s.as_str() == *c))
        .collect();
    for cat in ordered {
        let cat = cat.to_string();
        if std::time::Instant::now() >= deadline {
            cats["_actors"] = json!(&actors);
            return save_and_requeue(ctx, import_id, &cats, lease).await;
        }
        // Cancellation is honored at category boundaries too — a long
        // walk must not run a cancelled session to `done`.
        if cancel_requested(&ctx.db, import_id).await? {
            return settle_cancelled(ctx, import_id).await;
        }
        let st = cats.get(&cat).cloned().unwrap_or(json!({}));
        if st.get("done").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match cat.as_str() {
            "following" | "blocks" | "mutes" => {
                if let Err(c) = relation_cat(
                    ctx, &client, &mut cats, &cat, &session, rel, account_id, deadline,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            "statuses" => {
                if let Err(c) = statuses_cat(
                    ctx, &client, &mut cats, &session, import_id, account_id, federate, cutoff,
                    deadline, &source_id,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            "bookmarks" | "favourites" => {
                if let Err(c) = collection_cat(
                    ctx, &client, &mut cats, &cat, import_id, account_id, deadline,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            "dms" => {
                if let Err(c) = dms_cat(
                    ctx,
                    &client,
                    &mut cats,
                    &session,
                    import_id,
                    account_id,
                    &mut actors,
                    cutoff,
                    deadline,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            "profile" => {
                if let Err(c) = profile_cat(
                    ctx,
                    rel.ap,
                    &client,
                    &mut cats,
                    account_id,
                    session.overwrite_profile,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            "observations" => {
                if let Err(c) = observations_cat(
                    ctx,
                    &client,
                    &mut cats,
                    &session,
                    import_id,
                    account_id,
                    &mut actors,
                    &origin,
                    deadline,
                )
                .await
                {
                    cats["_actors"] = json!(&actors);
                    if handle_cat_error(ctx, import_id, &mut cats, c, &cat, lease)
                        .await?
                        .is_none()
                    {
                        return Ok(());
                    }
                }
            }
            _ => {
                cats[cat.as_str()] = json!({"done": true, "unsupported": true});
            }
        }
    }

    restore::fix_up_graph(&ctx.db, account_id).await?;
    // Enqueue the drain before committing `done` (see restore.rs) —
    // a retry of a still-`running` import re-enqueues it.
    if federate {
        crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs)
            .enqueue(JobKind::ImportPublish, json!({ "import_id": import_id }))
            .await?;
    }
    cats["_actors"] = json!(&actors);
    // A session whose categories ended in error still completes, but
    // says so in `error` instead of reading as a clean success.
    let failed: Vec<&str> = CATEGORIES
        .iter()
        .copied()
        .filter(|c| cats.get(c).and_then(|s| s.get("error")).is_some())
        .collect();
    let summary =
        (!failed.is_empty()).then(|| format!("completed with errors: {}", failed.join(", ")));
    // Conditional transition: a superseded runner (lost lease) leaves
    // the owner's state alone; only a cancel that landed mid-run settles.
    let n = sqlx::query(
        "UPDATE archive_imports SET state = 'done', stage = 'done', completed_at = now(), \
         category_state = $2, error = $4 WHERE id = $1 AND run_token = $3 AND NOT cancel_requested",
    )
    .bind(import_id)
    .bind(&cats)
    .bind(lease)
    .bind(summary)
    .execute(&ctx.db)
    .await?
    .rows_affected();
    if n == 0 && cancel_requested(&ctx.db, import_id).await? {
        return settle_cancelled(ctx, import_id).await;
    }
    Ok(())
}

/// Pause with a user-facing reason — resume re-enqueues. Category
/// cursors are persisted when supplied so progress survives the
/// pause (previously an auth pause discarded the in-memory walk).
/// The lease fence keeps a superseded runner from pausing a run it
/// no longer owns.
pub(crate) async fn pause(
    ctx: &JobContext,
    import_id: i64,
    cats: Option<&Value>,
    lease: &str,
    reason: &str,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE archive_imports SET state = 'paused', error = $2, \
         category_state = COALESCE($3, category_state) WHERE id = $1 AND run_token = $4 \
         AND state = 'running' AND NOT cancel_requested",
    )
    .bind(import_id)
    .bind(reason)
    .bind(cats)
    .bind(lease)
    .execute(&ctx.db)
    .await?
    .rows_affected();
    if n == 0 && cancel_requested(&ctx.db, import_id).await? {
        return settle_cancelled(ctx, import_id).await;
    }
    Ok(())
}

/// Persist cursors + schedule a delayed retry (rate limit, 5xx).
/// A fenced-out write (a newer run owns the session) skips the
/// enqueue — the owning run schedules its own continuation.
async fn retry_later(
    ctx: &JobContext,
    import_id: i64,
    cats: &Value,
    lease: &str,
    delay: Duration,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE archive_imports SET category_state = $2, \
         next_retry_at = now() + ($3::bigint * interval '1 second'), updated_at = now() \
         WHERE id = $1 AND run_token = $4 AND state = 'running' AND NOT cancel_requested",
    )
    .bind(import_id)
    .bind(cats)
    .bind(delay.as_secs() as i64)
    .bind(lease)
    .execute(&ctx.db)
    .await?
    .rows_affected();
    if n == 0 {
        if cancel_requested(&ctx.db, import_id).await? {
            return settle_cancelled(ctx, import_id).await;
        }
        return Ok(());
    }
    crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs)
        .enqueue_delayed(JobKind::Import, json!({ "import_id": import_id }), delay)
        .await?;
    Ok(())
}

/// Persist cursors and continue immediately — same lease fence as
/// [`retry_later`].
async fn save_and_requeue(
    ctx: &JobContext,
    import_id: i64,
    cats: &Value,
    lease: &str,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE archive_imports SET category_state = $2, next_retry_at = now(), \
         updated_at = now() WHERE id = $1 AND run_token = $3 \
         AND state = 'running' AND NOT cancel_requested",
    )
    .bind(import_id)
    .bind(cats)
    .bind(lease)
    .execute(&ctx.db)
    .await?
    .rows_affected();
    if n == 0 {
        if cancel_requested(&ctx.db, import_id).await? {
            return settle_cancelled(ctx, import_id).await;
        }
        return Ok(());
    }
    crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs)
        .enqueue(JobKind::Import, json!({ "import_id": import_id }))
        .await?;
    Ok(())
}

/// The cooperative cancel flag, checked at category boundaries.
pub(crate) async fn cancel_requested(db: &sqlx::PgPool, import_id: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, Option<bool>>(
        "SELECT cancel_requested FROM archive_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_optional(db)
    .await?
    .flatten()
    .unwrap_or(false))
}

/// Honor the cancel flag: flip to `cancelled` (unfenced — the flag
/// is a user action any runner may settle) and fail pending
/// publication work so nothing remains queued for a dead session.
pub(crate) async fn settle_cancelled(ctx: &JobContext, import_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE archive_imports SET state = 'cancelled', completed_at = now() \
         WHERE id = $1 AND cancel_requested AND state IN ('pending','running','paused')",
    )
    .bind(import_id)
    .execute(&ctx.db)
    .await?;
    super::publish::fail_pending_publication(&ctx.db, import_id, "import cancelled").await
}

/// How a category fetch's error class shapes the session state.
enum CatError {
    Auth,
    Transient(Option<Duration>),
    Unsupported,
    Validation(String),
    Malformed(String),
}

/// Capability 404s never reach here — `get_shaped`/`post_public` hand
/// them back as `unsupported` pages — so every error maps directly.
fn to_cat(e: RemoteError) -> CatError {
    match e {
        RemoteError::Auth => CatError::Auth,
        RemoteError::Transient(d, _) => CatError::Transient(d),
        RemoteError::Validation(m) => CatError::Validation(m),
        RemoteError::Malformed(m) => CatError::Malformed(m),
    }
}

/// Resolve one category error: `Some` keeps the run going (the
/// category is marked done with its terminal state), `None` means
/// the run returned early via pause/retry and `cats` was persisted.
async fn handle_cat_error(
    ctx: &JobContext,
    import_id: i64,
    cats: &mut Value,
    e: CatError,
    cat: &str,
    lease: &str,
) -> Result<Option<()>> {
    match e {
        CatError::Auth => {
            pause(
                ctx,
                import_id,
                Some(&*cats),
                lease,
                "remote authorization revoked — reauthorize to resume",
            )
            .await?;
        }
        CatError::Transient(d) => {
            retry_later(
                ctx,
                import_id,
                cats,
                lease,
                d.unwrap_or(Duration::from_secs(60)),
            )
            .await?;
        }
        CatError::Unsupported => {
            cats[cat] = json!({"done": true, "unsupported": true});
            return Ok(Some(()));
        }
        CatError::Validation(m) => {
            cats[cat] = json!({"done": true, "error": m});
            return Ok(Some(()));
        }
        CatError::Malformed(m) => {
            // Retry the same cursor; only repeated failures on one page
            // end the category, and the error is recorded (the session
            // then completes with errors rather than silently).
            let at = cat_state(cats, cat)
                .get("next")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let st = cat_state(cats, cat);
            let tries = match st.get("malformed_at").and_then(Value::as_str) {
                Some(prev) if prev == at => {
                    st.get("malformed_tries")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        + 1
                }
                _ => 1,
            };
            cats[cat]["malformed_at"] = json!(at);
            cats[cat]["malformed_tries"] = json!(tries);
            if tries < MAX_MALFORMED_TRIES {
                retry_later(ctx, import_id, cats, lease, MALFORMED_RETRY).await?;
                return Ok(None);
            }
            cats[cat]["done"] = json!(true);
            cats[cat]["error"] = json!(m);
            return Ok(Some(()));
        }
    }
    Ok(None)
}

fn cat_state(cats: &Value, cat: &str) -> Value {
    cats.get(cat).cloned().unwrap_or(json!({}))
}

fn bump(cats: &mut Value, cat: &str, field: &str, n: i64) {
    let cur = cats
        .get(cat)
        .and_then(|c| c.get(field))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    cats[cat][field] = json!(cur + n);
}

/// following/blocks/mutes: paginated account lists → real
/// relationships (Follow/Block/Mute) through the shared writers.
#[allow(clippy::too_many_arguments)]
async fn relation_cat(
    ctx: &JobContext,
    client: &Provider,
    cats: &mut Value,
    cat: &str,
    session: &Session,
    rel: relationships::Ctx<'_>,
    account_id: i64,
    deadline: std::time::Instant,
) -> std::result::Result<(), CatError> {
    let st = cat_state(cats, cat);
    let source_id = session.source_id.clone();
    // Blocks/mutes are account-level lists, not account/{id}
    // subresources — only following lives under the source account.
    let first = match cat {
        "following" => format!("/api/v1/accounts/{source_id}/following?limit={PAGE}"),
        "blocks" => format!("/api/v1/blocks?limit={PAGE}"),
        _ => format!("/api/v1/mutes?limit={PAGE}"),
    };
    let source_host = client.origin_host();
    let mut next = st
        .get("next")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or(first);
    for _ in 0..PAGES_PER_RUN {
        if std::time::Instant::now() >= deadline {
            cats[cat]["next"] = json!(next);
            return Err(CatError::Transient(Some(Duration::ZERO)));
        }
        let page = client.get_list(&next).await.map_err(to_cat)?;
        if page.unsupported {
            return Err(CatError::Unsupported);
        }
        if page.items.is_empty() {
            cats[cat] = merge_done(cats, cat);
            return Ok(());
        }
        for acct in &page.items {
            // A bare `acct` names a source-local account — qualify it
            // against the source origin or we'd resolve a same-named
            // destination-local account instead.
            let raw = acct.get("acct").and_then(Value::as_str).unwrap_or_default();
            if raw.is_empty() {
                continue;
            }
            let handle = if raw.contains('@') {
                raw.to_string()
            } else {
                format!("{raw}@{source_host}")
            };
            let r: Result<()> = match cat {
                "following" => {
                    let target = relationships::resolve_account(rel, &handle).await;
                    match target {
                        Ok(t) => relationships::follow(rel, account_id, t, FollowOpts::default())
                            .await
                            .map(|_| ()),
                        Err(e) => Err(e),
                    }
                }
                "blocks" => {
                    let target = relationships::resolve_account(rel, &handle).await;
                    match target {
                        Ok(t) => relationships::block(&ctx.db, account_id, t)
                            .await
                            .map(|_| ()),
                        Err(e) => Err(e),
                    }
                }
                _ => {
                    let target = relationships::resolve_account(rel, &handle).await;
                    match target {
                        Ok(t) => relationships::mute(&ctx.db, account_id, t, true, None).await,
                        Err(e) => Err(e),
                    }
                }
            };
            match r {
                Ok(()) => bump(cats, cat, "applied", 1),
                // An unresolvable handle or invalid item is a
                // permanent per-item failure — count and continue.
                // Anything else (DB, storage) is transient: retry the
                // page rather than losing the rest of it.
                Err(Error::Validation(m)) | Err(Error::NotFound(m)) => {
                    tracing::debug!(cat, handle, error = %m, "relation import failed");
                    bump(cats, cat, "failed", 1);
                }
                Err(e) => {
                    tracing::warn!(cat, handle, error = %e, "relation import interrupted; retrying");
                    return Err(CatError::Transient(Some(Duration::from_secs(60))));
                }
            }
        }
        match page.next {
            Some(n) => next = n,
            None => {
                cats[cat] = merge_done(cats, cat);
                return Ok(());
            }
        }
        cats[cat]["next"] = json!(next);
    }
    cats[cat]["next"] = json!(next);
    Err(CatError::Transient(Some(Duration::ZERO)))
}

fn merge_done(cats: &Value, cat: &str) -> Value {
    let mut st = cat_state(cats, cat);
    st["done"] = json!(true);
    st
}

/// The authored-activity stream: statuses, replies and boosts split
/// by shape — one paginated walk, primary-cap enforced, time-range
/// filtered, frozen at the session cutoff.
#[allow(clippy::too_many_arguments)]
async fn statuses_cat(
    ctx: &JobContext,
    client: &Provider,
    cats: &mut Value,
    session: &Session,
    import_id: i64,
    account_id: i64,
    federate: bool,
    cutoff: Option<DateTime<Utc>>,
    deadline: std::time::Instant,
    source_id: &str,
) -> std::result::Result<(), CatError> {
    let st = cat_state(cats, "statuses");
    let mut next = st
        .get("next")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            format!("/api/v1/accounts/{source_id}/statuses?limit={PAGE}&pinned=false")
        });
    // `idmap` survives across batches: source numeric id → AP URI,
    // so reply linkage resolves even when the parent sits on a
    // different page.
    let mut idmap: HashMap<String, String> = st
        .get("idmap")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mut applied_primary = st.get("applied").and_then(Value::as_i64).unwrap_or(0);
    if !cats["statuses"]["subjects"].is_object() {
        cats["statuses"]["subjects"] = json!({});
    }
    let oldest = session.order == "oldest";

    for _ in 0..PAGES_PER_RUN {
        if std::time::Instant::now() >= deadline || applied_primary >= session.limit {
            break;
        }
        let page = client.get_list(&next).await.map_err(to_cat)?;
        if page.unsupported {
            return Err(CatError::Unsupported);
        }
        if page.items.is_empty() {
            cats["statuses"] = merge_done(cats, "statuses");
            return Ok(());
        }
        // Providers walk newest-first: once the newest item on a page
        // predates `since`, every later page does too — stop instead
        // of scanning the account's whole older history.
        if let Some(sn) = session.since {
            let page_newest = page
                .items
                .iter()
                .filter_map(|s| {
                    s.get("created_at")
                        .and_then(Value::as_str)
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                })
                .max();
            if page_newest.is_some_and(|c| c.with_timezone(&Utc) < sn) {
                cats["statuses"] = merge_done(cats, "statuses");
                cats["statuses"]["idmap"] = json!(idmap);
                return Ok(());
            }
        }
        // Collect the page's id→uri first so same-page replies link.
        for s in &page.items {
            if let (Some(id), Some(uri)) = (
                s.get("id").and_then(|i| {
                    i.as_str()
                        .map(String::from)
                        .or_else(|| i.as_i64().map(|n| n.to_string()))
                }),
                s.get("uri").and_then(Value::as_str),
            ) {
                if idmap.len() < 20_000 {
                    idmap.insert(id, uri.to_string());
                }
            }
        }
        let items: Vec<&Value> = if oldest {
            page.items.iter().rev().collect()
        } else {
            page.items.iter().collect()
        };
        for s in items {
            if applied_primary >= session.limit {
                break;
            }
            // Direct statuses belong to the `dms` category — when it
            // isn't selected, private content must not be imported
            // through the statuses walk.
            if !session.categories.iter().any(|c| c.as_str() == "dms")
                && s.get("visibility").and_then(Value::as_str) == Some("direct")
            {
                bump(cats, "statuses", "skipped", 1);
                continue;
            }
            let created = s
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            if let Some(c) = created {
                if let Some(cut) = cutoff {
                    if c > cut {
                        continue; // arrived after the frozen cutoff
                    }
                }
                if let Some(u) = session.until {
                    if c >= u {
                        continue;
                    }
                }
                if let Some(sn) = session.since {
                    if c < sn {
                        continue;
                    }
                }
            }
            let item = synthesize(s, &idmap);
            if item.is_null() {
                bump(cats, "statuses", "skipped", 1);
                continue;
            }
            let mut observed = false;
            match restore::import_outbox_item(ctx, None, import_id, account_id, &item, federate)
                .await
            {
                // Only real source items consume the primary cap —
                // skipped/unsupported shapes don't.
                Ok(restore::ItemOutcome::Applied) => {
                    observed = true;
                    bump(cats, "statuses", "applied", 1);
                    applied_primary += 1;
                }
                Ok(restore::ItemOutcome::Partial(n)) => {
                    observed = true;
                    bump(cats, "statuses", "applied", 1);
                    bump(cats, "statuses", "media_unavailable", n);
                    applied_primary += 1;
                }
                // A dedupe hit is not a new import: it never consumes
                // the cap, so a retried page can't exhaust it twice.
                Ok(restore::ItemOutcome::Present) => {
                    observed = true;
                    bump(cats, "statuses", "present", 1);
                }
                Ok(restore::ItemOutcome::Skipped) => bump(cats, "statuses", "skipped", 1),
                Ok(restore::ItemOutcome::Unsupported) => bump(cats, "statuses", "unsupported", 1),
                // A malformed item is a permanent per-item failure;
                // any other error is transient — retry the page
                // (item writes are idempotent) instead of losing it.
                Err(Error::Validation(m)) => {
                    tracing::debug!(error = %m, "status import failed");
                    bump(cats, "statuses", "failed", 1);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "status import interrupted; retrying");
                    return Err(CatError::Transient(Some(Duration::from_secs(60))));
                }
            }
            if observed
                && s.get("reblog").is_none_or(Value::is_null)
                && s.get("visibility").and_then(Value::as_str) != Some("direct")
            {
                if let Some(uri) = s.get("uri").and_then(Value::as_str) {
                    cats["statuses"]["subjects"][uri] = json!(true);
                }
            }
        }
        match page.next {
            Some(n) => next = n,
            None => {
                cats["statuses"] = merge_done(cats, "statuses");
                cats["statuses"]["idmap"] = json!(idmap);
                return Ok(());
            }
        }
        cats["statuses"]["next"] = json!(next);
        cats["statuses"]["idmap"] = json!(idmap);
    }
    if applied_primary >= session.limit {
        cats["statuses"] = merge_done(cats, "statuses");
        cats["statuses"]["truncated_by_limit"] = json!(true);
    }
    cats["statuses"]["next"] = json!(next);
    cats["statuses"]["idmap"] = json!(idmap);
    Err(CatError::Transient(Some(Duration::ZERO)))
}

/// Mastodon REST Status → the AP item shape `import_outbox_item`
/// already consumes (Create/Note|Question, Announce).
fn synthesize(s: &Value, idmap: &HashMap<String, String>) -> Value {
    let uri = s.get("uri").and_then(Value::as_str).unwrap_or_default();
    if uri.is_empty() {
        return Value::Null;
    }
    let published = s.get("created_at").cloned().unwrap_or(Value::Null);
    if let Some(reblog) = s.get("reblog").filter(|r| !r.is_null()) {
        let target = reblog
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if target.is_empty() {
            return Value::Null;
        }
        return json!({
            "type": "Announce",
            "id": uri,
            "published": published,
            "object": target,
            "to": audience_for(s).0,
            "cc": audience_for(s).1,
        });
    }
    let actor = s
        .get("account")
        .and_then(|a| a.get("uri").or_else(|| a.get("url")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let in_reply_to = s
        .get("in_reply_to_id")
        .and_then(|i| {
            i.as_str()
                .map(String::from)
                .or_else(|| i.as_i64().map(|n| n.to_string()))
        })
        .and_then(|id| idmap.get(&id).cloned());
    let mut attachment: Vec<Value> = Vec::new();
    if let Some(media) = s.get("media_attachments").and_then(Value::as_array) {
        for m in media.iter().take(8) {
            // `remote_url` is null for source-local attachments —
            // fall back through nonempty strings, not key presence.
            let url = ["remote_url", "url", "preview_url"]
                .iter()
                .filter_map(|k| m.get(k).and_then(Value::as_str))
                .find(|u| !u.is_empty())
                .unwrap_or_default();
            if !url.is_empty() {
                attachment.push(json!({
                    "url": url,
                    "mediaType": m.get("type").and_then(Value::as_str).map(|t| format!("{t}/*")).unwrap_or_default(),
                    "name": m.get("description"),
                }));
            }
        }
    }
    let mut tag: Vec<Value> = Vec::new();
    let mut mention_hrefs: Vec<Value> = Vec::new();
    if let Some(mentions) = s.get("mentions").and_then(Value::as_array) {
        for m in mentions {
            if let Some(url) = m.get("url").and_then(Value::as_str) {
                mention_hrefs.push(json!(url));
                tag.push(json!({"type": "Mention", "href": url}));
            }
        }
    }
    if let Some(tags) = s.get("tags").and_then(Value::as_array) {
        for t in tags {
            if let Some(name) = t.get("name").and_then(Value::as_str) {
                tag.push(json!({"type": "Hashtag", "name": name}));
            }
        }
    }
    let (mut to, cc) = audience_for(s);
    // Mastodon carries a direct message's recipients as mentions —
    // an empty audience would store a recipient-less history row.
    if s.get("visibility").and_then(Value::as_str) == Some("direct") {
        to = json!(mention_hrefs);
    }
    let mut obj = json!({
        "type": if s.get("poll").is_some() { "Question" } else { "Note" },
        "id": uri,
        "attributedTo": actor,
        "published": published,
        "content": s.get("content").and_then(Value::as_str).unwrap_or_default(),
        "contentMap": s.get("language").and_then(Value::as_str).map(|l| json!({l: s.get("content").and_then(Value::as_str).unwrap_or_default()})),
        "summary": s.get("spoiler_text"),
        "sensitive": s.get("sensitive"),
        "to": to,
        "cc": cc,
        "attachment": attachment,
        "tag": tag,
        "inReplyTo": in_reply_to,
    });
    if let Some(poll) = s.get("poll") {
        if let Some(options) = poll.get("options").and_then(Value::as_array) {
            let multiple = poll
                .get("multiple")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let key = if multiple { "anyOf" } else { "oneOf" };
            obj[key] = json!(options
                .iter()
                .map(|o| json!({
                    "name": o.get("title"),
                    "replies": {"totalItems": o.get("votes_count").and_then(Value::as_i64).unwrap_or(0)},
                }))
                .collect::<Vec<_>>());
        }
        obj["endTime"] = poll.get("expires_at").cloned().unwrap_or(Value::Null);
    }
    let mut item = json!({
        "type": "Create",
        "id": format!("{uri}#create"),
        "actor": actor,
        "published": published,
        "object": obj,
    });
    // Source-side counters become the status's remote_*_count
    // provenance columns on import.
    for (src, dst) in [
        ("favourites_count", "yonder_likes_count"),
        ("reblogs_count", "yonder_shares_count"),
        ("quotes_count", "yonder_quotes_count"),
    ] {
        if let Some(n) = s.get(src).and_then(Value::as_i64) {
            item[dst] = json!(n);
        }
    }
    item
}

/// Mastodon REST `visibility` → (to, cc) the restore path reads.
fn audience_for(s: &Value) -> (Value, Value) {
    let followers = s
        .get("account")
        .and_then(|a| a.get("url"))
        .and_then(Value::as_str)
        .map(|u| format!("{u}/followers"))
        .unwrap_or_default();
    match s
        .get("visibility")
        .and_then(Value::as_str)
        .unwrap_or("public")
    {
        "unlisted" => (json!([followers]), json!([super::AS_PUBLIC])),
        "private" => (json!([followers]), json!([])),
        "direct" => (json!([]), json!([])),
        _ => (json!([super::AS_PUBLIC]), json!([followers])),
    }
}

/// bookmarks/favourites: paginated statuses → the shared collection
/// writer (self-posts map to imported copies inside it).
async fn collection_cat(
    ctx: &JobContext,
    client: &Provider,
    cats: &mut Value,
    cat: &str,
    import_id: i64,
    account_id: i64,
    deadline: std::time::Instant,
) -> std::result::Result<(), CatError> {
    let mut next = cat_state(cats, cat)
        .get("next")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("/api/v1/{cat}?limit={PAGE}"));
    let favourite = cat == "favourites";
    for _ in 0..PAGES_PER_RUN {
        if std::time::Instant::now() >= deadline {
            cats[cat]["next"] = json!(next);
            return Err(CatError::Transient(Some(Duration::ZERO)));
        }
        let page = client.get_list(&next).await.map_err(to_cat)?;
        if page.unsupported {
            return Err(CatError::Unsupported);
        }
        if page.items.is_empty() {
            cats[cat] = merge_done(cats, cat);
            return Ok(());
        }
        for s in &page.items {
            let uri = s.get("uri").and_then(Value::as_str).unwrap_or_default();
            if uri.is_empty() {
                continue;
            }
            match restore::apply_collection_item(ctx, account_id, import_id, uri, favourite).await {
                Ok(restore::ItemOutcome::Applied | restore::ItemOutcome::Partial(_)) => {
                    bump(cats, cat, "applied", 1)
                }
                Ok(restore::ItemOutcome::Present) => bump(cats, cat, "present", 1),
                Ok(_) => bump(cats, cat, "skipped", 1),
                Err(Error::Validation(m)) => {
                    tracing::debug!(cat, uri, error = %m, "collection import failed");
                    bump(cats, cat, "failed", 1);
                }
                Err(e) => {
                    tracing::warn!(cat, uri, error = %e, "collection import interrupted; retrying");
                    return Err(CatError::Transient(Some(Duration::from_secs(60))));
                }
            }
        }
        match page.next {
            Some(n) => next = n,
            None => {
                cats[cat] = merge_done(cats, cat);
                return Ok(());
            }
        }
        cats[cat]["next"] = json!(next);
    }
    cats[cat]["next"] = json!(next);
    Err(CatError::Transient(Some(Duration::ZERO)))
}

/// conversations → owner-only `private_history_messages` for received
/// DMs (the source API only exposes each thread's last message — the
/// summary records that limitation, never a complete-history claim).
#[allow(clippy::too_many_arguments)]
async fn dms_cat(
    ctx: &JobContext,
    client: &Provider,
    cats: &mut Value,
    session: &Session,
    import_id: i64,
    account_id: i64,
    actors: &mut HashMap<String, Option<String>>,
    cutoff: Option<DateTime<Utc>>,
    deadline: std::time::Instant,
) -> std::result::Result<(), CatError> {
    let mut next = cat_state(cats, "dms")
        .get("next")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("/api/v1/conversations?limit={PAGE}"));
    for _ in 0..PAGES_PER_RUN {
        if std::time::Instant::now() >= deadline {
            cats["dms"]["next"] = json!(next);
            return Err(CatError::Transient(Some(Duration::ZERO)));
        }
        let page = client.get_list(&next).await.map_err(to_cat)?;
        if page.unsupported {
            return Err(CatError::Unsupported);
        }
        if page.items.is_empty() {
            cats["dms"] = merge_done(cats, "dms");
            return Ok(());
        }
        for c in &page.items {
            let Some(last) = c.get("last_status") else {
                continue;
            };
            if last.is_null() {
                continue;
            }
            let sender_api_id = last
                .get("account")
                .and_then(|a| a.get("id"))
                .and_then(|i| {
                    i.as_str()
                        .map(String::from)
                        .or_else(|| i.as_i64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            // Range/cutoff filters apply to DM rows too.
            let created = last
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            if let Some(c) = created {
                if cutoff.is_some_and(|cut| c > cut)
                    || session.until.is_some_and(|u| c >= u)
                    || session.since.is_some_and(|s| c < s)
                {
                    continue;
                }
            }
            let uri = last.get("uri").and_then(Value::as_str).unwrap_or_default();
            if uri.is_empty() {
                continue;
            }
            // A self-authored message is only covered when the statuses
            // walk actually imported it (it may have been out of range
            // or past the limit there); otherwise it must come through
            // here. Direct items are recorded as kind `dm`.
            if sender_api_id == session.source_id
                && session.categories.iter().any(|c| c.as_str() == "statuses")
            {
                let covered: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM import_items \
                     WHERE import_id = $1 AND source_uri = $2 \
                       AND kind IN ('post','reply','dm'))",
                )
                .bind(import_id)
                .bind(uri)
                .fetch_one(&ctx.db)
                .await
                .map_err(|e| db_retry(e.into()))?;
                if covered {
                    continue;
                }
            }
            // The sender is `last_status.account`, not the first
            // participant (wrong for group conversations); the other
            // participants are recipients.
            let sender_uri = resolve_actor(
                client,
                actors,
                &last.get("account").cloned().unwrap_or(json!({})),
            )
            .await?;
            // `accounts` is the *other* participants — the sender's own
            // entry is excluded, and a received message's recipients
            // include the importing source account.
            let mut recipient_uris: Vec<String> = c
                .get("accounts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|a| {
                    a.get("id")
                        .and_then(|i| {
                            i.as_str()
                                .map(String::from)
                                .or_else(|| i.as_i64().map(|n| n.to_string()))
                        })
                        .as_deref()
                        != Some(sender_api_id.as_str())
                })
                .filter_map(|a| {
                    a.get("url")
                        .or_else(|| a.get("uri"))
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect();
            if sender_api_id != session.source_id && !session.source_actor_uri.is_empty() {
                recipient_uris.push(session.source_actor_uri.clone());
            }
            let media_keys: Vec<String> = last
                .get("media_attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    ["remote_url", "url", "preview_url"]
                        .iter()
                        .filter_map(|k| m.get(k).and_then(Value::as_str))
                        .find(|u| !u.is_empty())
                        .map(String::from)
                })
                .collect();
            let body = last
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let body_html = crate::posts::render::sanitize_html(body, &ctx.config);
            let original_at = last
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            let source_origin = url::Url::parse(uri)
                .ok()
                .and_then(|u| u.host_str().map(String::from));
            // A message already recorded from another conversation entry
            // keeps its attribution; its recipients and media merge in.
            let res = sqlx::query_scalar::<_, bool>(
                "INSERT INTO private_history_messages \
                   (owner_id, import_id, sender_uri, sender_account_id, recipient_uris, \
                    body, media_keys, source_uri, source_origin, original_at) \
                 VALUES ($1, $2, $3, (SELECT id FROM accounts WHERE actor_id = $3), \
                         $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (owner_id, source_uri) DO UPDATE SET \
                   recipient_uris = (SELECT COALESCE(jsonb_agg(DISTINCT u), '[]'::jsonb) \
                       FROM jsonb_array_elements_text(private_history_messages.recipient_uris \
                                                      || EXCLUDED.recipient_uris) AS u), \
                   media_keys = (SELECT COALESCE(jsonb_agg(DISTINCT u), '[]'::jsonb) \
                       FROM jsonb_array_elements_text(private_history_messages.media_keys \
                                                      || EXCLUDED.media_keys) AS u) \
                 RETURNING (xmax = 0)",
            )
            .bind(account_id)
            .bind(import_id)
            .bind(&sender_uri)
            .bind(json!(recipient_uris))
            .bind(&body_html)
            .bind(json!(media_keys))
            .bind(uri)
            .bind(&source_origin)
            .bind(original_at)
            .fetch_one(&ctx.db)
            .await;
            match res {
                Ok(true) => bump(cats, "dms", "applied", 1),
                Ok(false) => bump(cats, "dms", "merged", 1),
                // Transient writes retry the page — a failed insert
                // must not advance past the message.
                Err(e) => return Err(db_retry(e.into())),
            }
        }
        match page.next {
            Some(n) => next = n,
            None => {
                cats["dms"] = merge_done(cats, "dms");
                cats["dms"]["note"] = json!("conversations expose only each thread's last message");
                return Ok(());
            }
        }
        cats["dms"]["next"] = json!(next);
    }
    cats["dms"]["next"] = json!(next);
    Err(CatError::Transient(Some(Duration::ZERO)))
}

/// Best-effort actor URI for a REST account (webfinger, capped).
/// Bare source-local `acct`s are qualified with the source host —
/// `acct:name` resolves on the destination instance otherwise. A
/// transient lookup failure fails the category (retried at its
/// cursor); only a definitive answer is cached for the session.
async fn resolve_actor(
    client: &Provider,
    actors: &mut HashMap<String, Option<String>>,
    account: &Value,
) -> std::result::Result<String, CatError> {
    let fallback = || {
        account
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let acct = account
        .get("acct")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if acct.is_empty() {
        return Ok(fallback());
    }
    let resource = if acct.contains('@') {
        format!("acct:{acct}")
    } else {
        format!("acct:{acct}@{}", client.origin_host())
    };
    if actors.len() >= MAX_ACTOR_RESOLUTIONS && !actors.contains_key(&resource) {
        return Ok(fallback());
    }
    match actors.get(&resource) {
        Some(Some(u)) => Ok(u.clone()),
        Some(None) => Ok(fallback()),
        None => {
            let page = client
                .get_doc(&format!(
                    "/.well-known/webfinger?resource={}",
                    urlenc(&resource)
                ))
                .await
                .map_err(to_cat)?;
            let resolved = page
                .items
                .into_iter()
                .next()
                .and_then(|d| webfinger_self(&d, &resource));
            actors.insert(resource, resolved.clone());
            Ok(resolved.unwrap_or_else(fallback))
        }
    }
}

/// profile: display name + bio + fields, only when selected; link
/// verification metadata is dropped (a migrated profile is not a
/// verified one). By default the import merges: an absent or empty
/// remote value keeps the destination's. `overwrite` lets present
/// (even empty) remote values replace it.
async fn profile_cat(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    client: &Provider,
    cats: &mut Value,
    account_id: i64,
    overwrite: bool,
) -> std::result::Result<(), CatError> {
    let page = client
        .get_doc("/api/v1/accounts/verify_credentials")
        .await
        .map_err(to_cat)?;
    if page.unsupported {
        return Err(CatError::Unsupported);
    }
    // Identity must validate before any field is trusted: an
    // `{"error": "…"}` shaped success carries no account and must
    // not erase the destination profile.
    let Some(v) = page.items.into_iter().next().filter(Value::is_object) else {
        return Err(CatError::Validation(
            "verify_credentials returned no account".into(),
        ));
    };
    if account_fields(&v).is_none() {
        return Err(CatError::Validation(
            "verify_credentials returned no usable account".into(),
        ));
    }
    // Merge keeps the destination for absent/empty remote values; only
    // an explicit overwrite lets an empty source profile clear it.
    let keep = |s: &String| overwrite || !s.is_empty();
    let display = v
        .get("display_name")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(200).collect::<String>())
        .filter(keep);
    let bio = v
        .get("note")
        .and_then(Value::as_str)
        .map(|n| {
            crate::posts::render::html_to_text(n)
                .chars()
                .take(2000)
                .collect::<String>()
        })
        .filter(keep);
    let fields = v
        .get("fields")
        .and_then(Value::as_array)
        .filter(|a| overwrite || !a.is_empty())
        .map(|a| {
            let inputs: Vec<crate::users::profile::FieldInput> = a
                .iter()
                .map(|f| crate::users::profile::FieldInput {
                    name: f
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    value: f
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            crate::users::profile::validate_fields(&ctx.config, &inputs)?;
            crate::users::profile::render_fields(&ctx.config, &inputs, &[])
        })
        .transpose()
        .map_err(|e| CatError::Validation(e.to_string()))?;
    // One statement — the profile never half-applies, and a database
    // failure retries instead of marking the category malformed.
    sqlx::query(
        "UPDATE accounts SET display_name = COALESCE($2, display_name), \
                bio = COALESCE($3, bio), \
                profile_fields = COALESCE($4::jsonb, profile_fields) \
         WHERE id = $1",
    )
    .bind(account_id)
    .bind(&display)
    .bind(&bio)
    .bind(fields.map(|f| json!(f)))
    .execute(&ctx.db)
    .await
    .map_err(|e| db_retry(e.into()))?;
    if let Some(ap) = ap {
        let username: Option<String> =
            sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1 AND domain IS NULL")
                .bind(account_id)
                .fetch_optional(&ctx.db)
                .await
                .map_err(|e| db_retry(e.into()))?;
        if let Some(username) = username {
            crate::activitypub::actors::invalidate_handlers(&ap.handlers, Some(&username));
            // The document set pubby invalidates on an actor update,
            // scoped to this username.
            ap.core.doc_cache.invalidate_prefix(&[
                pubby_sdk::cache::actor_document_key_for(&username),
                pubby_sdk::cache::webfinger_key(&format!(
                    "{username}@{}",
                    ap.multi.webfinger_domain
                )),
                pubby_sdk::cache::route_key(&["outbox", &username]),
                pubby_sdk::cache::followers_key_for(&username),
                pubby_sdk::cache::following_key_for(&username),
            ]);
        }
    }
    cats["profile"] = merge_done(cats, "profile");
    Ok(())
}

/// favourited_by/reblogged_by/context/quotes → bounded
/// `historical_observations` — provenance records, never fabricated
/// live interactions and never invented ids/timestamps.
#[allow(clippy::too_many_arguments)]
async fn observations_cat(
    ctx: &JobContext,
    client: &Provider,
    cats: &mut Value,
    _session: &Session,
    import_id: i64,
    account_id: i64,
    actors: &mut HashMap<String, Option<String>>,
    origin: &str,
    deadline: std::time::Instant,
) -> std::result::Result<(), CatError> {
    let st = cat_state(cats, "observations");
    let mut offset = st.get("offset").and_then(Value::as_i64).unwrap_or(0);
    // Limit+1 detects truncation honestly — a source with more
    // subjects than the cap reports it instead of silently stopping.
    let selected = cats["statuses"]["subjects"].as_object();
    let source_uris: Vec<String> = selected
        .into_iter()
        .flat_map(|m| m.keys().cloned())
        .collect();
    let subjects: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, original_id FROM statuses \
         WHERE account_id = $3 AND NOT tombstone AND \
           (original_id = ANY($4) OR ($5 AND id IN \
             (SELECT target_status_id FROM import_items WHERE import_id = $1 \
              AND kind IN ('post','reply')))) \
         ORDER BY id LIMIT $2",
    )
    .bind(import_id)
    .bind(MAX_OBSERVED_STATUSES as i64 + 1)
    .bind(account_id)
    .bind(source_uris)
    .bind(selected.is_none())
    .fetch_all(&ctx.db)
    .await
    .map_err(|_| CatError::Transient(Some(Duration::from_secs(60))))?;
    let (subjects, subject_truncated) = if subjects.len() > MAX_OBSERVED_STATUSES {
        (subjects[..MAX_OBSERVED_STATUSES].to_vec(), true)
    } else {
        (subjects, false)
    };
    if subject_truncated {
        cats["observations"]["truncated"] = json!(true);
    }
    let uri_to_id: HashMap<String, String> = cats
        .get("statuses")
        .and_then(|c| c.get("idmap"))
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (s.to_string(), k.clone())))
                .collect()
        })
        .unwrap_or_default();

    while (offset as usize) < subjects.len() {
        if std::time::Instant::now() >= deadline {
            cats["observations"]["offset"] = json!(offset);
            return Err(CatError::Transient(Some(Duration::ZERO)));
        }
        let (_, source_uri) = &subjects[offset as usize];
        let api_id = uri_to_id.get(source_uri).cloned().unwrap_or_default();
        if api_id.is_empty() {
            offset += 1;
            continue;
        }
        for (kind, suffix) in [("like", "favourited_by"), ("boost", "reblogged_by")] {
            let mut next = Some(format!("/api/v1/statuses/{api_id}/{suffix}?limit={PAGE}"));
            let mut pages = 0;
            while let Some(path) = next.take() {
                // The deadline applies inside the page loop too — one
                // expensive subject must not run past the worker
                // timeout or restart forever.
                if std::time::Instant::now() >= deadline {
                    cats["observations"]["offset"] = json!(offset);
                    return Err(CatError::Transient(Some(Duration::ZERO)));
                }
                if pages == OBSERVATION_PAGES {
                    cats["observations"]["truncated"] = json!(true);
                    break;
                }
                pages += 1;
                let page = client.get_list(&path).await.map_err(to_cat)?;
                if page.unsupported {
                    break;
                }
                for acct in &page.items {
                    let actor = resolve_actor(client, actors, acct).await?;
                    let dedupe = format!("{kind}:{source_uri}:{actor}");
                    if record_observation(
                        ctx,
                        account_id,
                        source_uri,
                        kind,
                        Some(&actor),
                        None,
                        None,
                        None,
                        origin,
                        "public",
                        &dedupe,
                    )
                    .await
                    .map_err(db_retry)?
                    {
                        bump(cats, "observations", kind, 1);
                    }
                }
                next = page.next;
            }
        }
        // Received replies + quotes ride the context/quotes
        // endpoints — bounded, and quotes capability-detects on 404.
        let ctx_page = client
            .get_doc(&format!("/api/v1/statuses/{api_id}/context"))
            .await
            .map_err(to_cat)?;
        if !ctx_page.unsupported {
            if let Some(desc) = ctx_page
                .items
                .first()
                .and_then(|d| d.get("descendants"))
                .and_then(Value::as_array)
            {
                if desc.len() > 50 {
                    cats["observations"]["truncated"] = json!(true);
                }
                for d in desc.iter().take(50) {
                    let uri = d.get("uri").and_then(Value::as_str).unwrap_or_default();
                    if uri.is_empty() {
                        continue;
                    }
                    let actor = resolve_actor(
                        client,
                        actors,
                        &d.get("account").cloned().unwrap_or(json!({})),
                    )
                    .await?;
                    let created = d
                        .get("created_at")
                        .and_then(Value::as_str)
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&Utc));
                    let dedupe = format!("reply:{source_uri}:{uri}");
                    if record_observation(
                        ctx,
                        account_id,
                        source_uri,
                        "reply",
                        Some(&actor),
                        None,
                        Some(uri),
                        created,
                        origin,
                        "public",
                        &dedupe,
                    )
                    .await
                    .map_err(db_retry)?
                    {
                        bump(cats, "observations", "reply", 1);
                    }
                }
            }
        }
        // Quotes paginate like the other observation endpoints —
        // a second page's `next` is followed, bounded, and marked
        // when the cap truncates.
        let mut q_next = Some(format!("/api/v1/statuses/{api_id}/quotes?limit={PAGE}"));
        let mut q_pages = 0;
        let mut q_unsupported = false;
        while let Some(path) = q_next.take() {
            if std::time::Instant::now() >= deadline {
                cats["observations"]["offset"] = json!(offset);
                return Err(CatError::Transient(Some(Duration::ZERO)));
            }
            if q_pages == OBSERVATION_PAGES {
                cats["observations"]["truncated"] = json!(true);
                break;
            }
            q_pages += 1;
            let q_page = client.get_list(&path).await.map_err(to_cat)?;
            if q_page.unsupported {
                q_unsupported = true;
                break;
            }
            for q in &q_page.items {
                let uri = q.get("uri").and_then(Value::as_str).unwrap_or_default();
                if uri.is_empty() {
                    continue;
                }
                let actor = resolve_actor(
                    client,
                    actors,
                    &q.get("account").cloned().unwrap_or(json!({})),
                )
                .await?;
                let created = q
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc));
                let dedupe = format!("quote:{source_uri}:{uri}");
                if record_observation(
                    ctx,
                    account_id,
                    source_uri,
                    "quote",
                    Some(&actor),
                    None,
                    Some(uri),
                    created,
                    origin,
                    "public",
                    &dedupe,
                )
                .await
                .map_err(db_retry)?
                {
                    bump(cats, "observations", "quote", 1);
                }
            }
            q_next = q_page.next;
        }
        if q_unsupported {
            cats["observations"]["quotes_unsupported"] = json!(true);
        }
        offset += 1;
        cats["observations"]["offset"] = json!(offset);
    }
    cats["observations"] = merge_done(cats, "observations");
    Ok(())
}

/// A failed observation write retries the category instead of
/// reporting it as applied.
fn db_retry(e: Error) -> CatError {
    tracing::warn!(error = %e, "observation write failed; retrying");
    CatError::Transient(Some(Duration::from_secs(60)))
}

/// One `historical_observations` row — dedupe key makes retries
/// converge; unknown actors/timestamps stay NULL, never invented.
/// Returns whether the row was newly inserted so retrying a
/// subject doesn't double-count already-recorded observations.
#[allow(clippy::too_many_arguments)]
async fn record_observation(
    ctx: &JobContext,
    owner_id: i64,
    source_post_uri: &str,
    kind: &str,
    actor_uri: Option<&str>,
    activity_uri: Option<&str>,
    object_uri: Option<&str>,
    original_at: Option<DateTime<Utc>>,
    source_origin: &str,
    visibility: &str,
    dedupe_key: &str,
) -> Result<bool> {
    let n = sqlx::query(
        "INSERT INTO historical_observations \
           (owner_id, status_id, source_post_uri, kind, actor_uri, activity_uri, \
            object_uri, original_at, source_origin, visibility, dedupe_key) \
         VALUES ($1, \
                 (SELECT id FROM statuses WHERE account_id = $1 AND original_id = $2), \
                 $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (owner_id, dedupe_key) DO NOTHING",
    )
    .bind(owner_id)
    .bind(source_post_uri)
    .bind(kind)
    .bind(actor_uri)
    .bind(activity_uri)
    .bind(object_uri)
    .bind(original_at)
    .bind(source_origin)
    .bind(visibility)
    .bind(dedupe_key)
    .execute(&ctx.db)
    .await?
    .rows_affected();
    Ok(n > 0)
}

// ---------------------------------------------------------------------------
// Historical reads (epic 31 §10)
// ---------------------------------------------------------------------------

/// `GET /api/v1/yonder/statuses/{id}/history` — owner-scoped
/// observations for one imported copy.
pub async fn status_history(state: &AppState, account_id: i64, status_id: i64) -> Result<Value> {
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE id = $1 AND account_id = $2)",
    )
    .bind(status_id)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    if !owned {
        return Err(Error::NotFound("status".into()));
    }
    let rows = sqlx::query(
        "SELECT kind, actor_uri, activity_uri, object_uri, observed_at, original_at, \
                source_origin, visibility \
         FROM historical_observations WHERE owner_id = $1 AND status_id = $2 \
         ORDER BY original_at NULLS LAST, id LIMIT 200",
    )
    .bind(account_id)
    .bind(status_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!(rows
        .iter()
        .map(|r| json!({
            "kind": r.get::<String, _>("kind"),
            "actor_uri": r.get::<Option<String>, _>("actor_uri"),
            "activity_uri": r.get::<Option<String>, _>("activity_uri"),
            "object_uri": r.get::<Option<String>, _>("object_uri"),
            "observed_at": r.get::<DateTime<Utc>, _>("observed_at").to_rfc3339(),
            "original_at": r.get::<Option<DateTime<Utc>>, _>("original_at").map(|t| t.to_rfc3339()),
            "source_origin": r.get::<Option<String>, _>("source_origin"),
            "visibility": r.get::<String, _>("visibility"),
        }))
        .collect::<Vec<_>>()))
}

/// `GET /api/v1/yonder/history/messages` — owner-only imported
/// private history, newest first, `max_id` paginated.
pub async fn history_messages(
    state: &AppState,
    account_id: i64,
    max_id: Option<i64>,
) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT id, sender_uri, sender_account_id, recipient_uris, body, media_keys, \
                source_uri, source_origin, original_at, created_at \
         FROM private_history_messages \
         WHERE owner_id = $1 AND ($2::bigint IS NULL OR id < $2) \
         ORDER BY id DESC LIMIT 100",
    )
    .bind(account_id)
    .bind(max_id)
    .fetch_all(&state.db)
    .await?;
    Ok(json!(rows
        .iter()
        .map(|r| json!({
            "id": r.get::<i64, _>("id").to_string(),
            "sender_uri": r.get::<String, _>("sender_uri"),
            "recipient_uris": r.get::<Value, _>("recipient_uris"),
            "body": r.get::<Option<String>, _>("body"),
            "media": history_media_projection(
                account_id, r.get::<i64, _>("id"), &r.get::<Value, _>("media_keys"),
            ),
            "source_uri": r.get::<String, _>("source_uri"),
            "source_origin": r.get::<Option<String>, _>("source_origin"),
            "original_at": r.get::<Option<DateTime<Utc>>, _>("original_at").map(|t| t.to_rfc3339()),
        }))
        .collect::<Vec<_>>()))
}

fn history_media_projection(owner: i64, id: i64, keys: &Value) -> Vec<Value> {
    keys.as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, value)| {
            let key = value.as_str()?;
            if let Ok(url) = url::Url::parse(key) {
                return (matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
                    .then(|| json!({"kind": "remote", "url": url.as_str()}));
            }
            if !private_history_key(owner, key) {
                return None;
            }
            Some(json!({
                "kind": "local",
                "url": format!("/api/v1/yonder/history/messages/{id}/media/{index}"),
            }))
        })
        .collect()
}

fn private_history_key(owner: i64, key: &str) -> bool {
    key.strip_prefix(&format!("imports/{owner}/"))
        .is_some_and(|name| {
            !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\'])
        })
}

pub async fn history_media(
    state: &AppState,
    account_id: i64,
    id: i64,
    index: i32,
) -> Result<Vec<u8>> {
    if index < 0 {
        return Err(Error::NotFound("history media".into()));
    }
    let key: Option<String> = sqlx::query_scalar(
        "SELECT media_keys ->> $3 FROM private_history_messages WHERE id = $1 AND owner_id = $2",
    )
    .bind(id)
    .bind(account_id)
    .bind(index)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let key = key.ok_or_else(|| Error::NotFound("history media".into()))?;
    if !private_history_key(account_id, &key) {
        return Err(Error::NotFound("history media".into()));
    }
    state.media.get(&key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_success_bodies_are_malformed_errors() {
        for body in [&b"<html>proxy</html>"[..], b"", b"{\"a\":"] {
            assert!(matches!(
                parse_page(200, None, None, body, Shape::List),
                Err(RemoteError::Malformed(_))
            ));
        }
    }

    #[test]
    fn json_success_bodies_parse_to_items() {
        let empty = parse_page(200, None, None, b"[]", Shape::List).unwrap();
        assert!(empty.items.is_empty() && !empty.unsupported);
        let one = parse_page(200, None, None, b"{\"id\":\"1\"}", Shape::Doc).unwrap();
        assert_eq!(one.items.len(), 1);
        let many = parse_page(200, None, Some("next".into()), b"[{},{}]", Shape::List).unwrap();
        assert_eq!((many.items.len(), many.next.as_deref()), (2, Some("next")));
    }

    #[test]
    fn wrong_shape_bodies_are_malformed_errors() {
        // A 200 object on a list endpoint (e.g. `{"error": "busy"}`)
        // is a failure, never a one-item page or end-of-collection.
        assert!(matches!(
            parse_page(200, None, None, b"{\"error\":\"busy\"}", Shape::List),
            Err(RemoteError::Malformed(_))
        ));
        // An array on a document endpoint is likewise refused.
        assert!(matches!(
            parse_page(200, None, None, b"[1,2]", Shape::Doc),
            Err(RemoteError::Malformed(_))
        ));
    }

    #[test]
    fn account_fields_requires_typed_identity() {
        assert_eq!(
            account_fields(&json!({"id": "src1", "acct": "olduser"})),
            Some(("src1".to_string(), "olduser".to_string()))
        );
        // Error-shaped and partial objects are not accounts.
        assert_eq!(account_fields(&json!({"error": "busy"})), None);
        assert_eq!(account_fields(&json!({"acct": "olduser"})), None);
        assert_eq!(account_fields(&json!({"id": "", "acct": "olduser"})), None);
        assert_eq!(account_fields(&json!({})), None);
    }

    #[test]
    fn webfinger_self_link_is_validated() {
        let resource = "acct:olduser@remote.example";
        let doc = json!({
            "subject": resource,
            "links": [{"rel": "self", "type": "application/activity+json",
                       "href": "https://remote.example/users/olduser"}],
        });
        assert_eq!(
            webfinger_self(&doc, resource).as_deref(),
            Some("https://remote.example/users/olduser")
        );
        // A mismatched subject is never trusted.
        let wrong = json!({
            "subject": "acct:other@remote.example",
            "links": [{"rel": "self", "type": "application/activity+json",
                       "href": "https://remote.example/users/other"}],
        });
        assert_eq!(webfinger_self(&wrong, resource), None);
        // A self link without the AP media type is not an actor.
        let profile = json!({
            "subject": resource,
            "links": [{"rel": "self", "type": "text/html",
                       "href": "https://remote.example/@olduser"}],
        });
        assert_eq!(webfinger_self(&profile, resource), None);
        // Non-URL hrefs are refused.
        let bogus = json!({
            "subject": resource,
            "links": [{"rel": "self", "type": "application/activity+json",
                       "href": "javascript:alert(1)"}],
        });
        assert_eq!(webfinger_self(&bogus, resource), None);
    }

    #[test]
    fn error_statuses_skip_the_body_and_classify() {
        let page = parse_page(
            503,
            Some(Duration::from_secs(9)),
            None,
            b"<html>",
            Shape::List,
        )
        .unwrap();
        assert!(page.items.is_empty());
        assert!(matches!(
            classify(&page),
            RemoteError::Transient(Some(d), _) if d == Duration::from_secs(9)
        ));
        let auth = parse_page(401, None, None, b"", Shape::List).unwrap();
        assert!(matches!(classify(&auth), RemoteError::Auth));
        let gone = parse_page(404, None, None, b"", Shape::List).unwrap();
        assert!(gone.unsupported);
    }

    #[test]
    fn binding_cookie_round_trips() {
        let set = binding_cookie("https://yonder.example/cb", "tok123");
        assert!(set.starts_with("yonder_import_oauth=tok123;"));
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Lax") && set.contains("; Secure")
        );
        let header = "theme=dark; yonder_import_oauth=tok123; other=1";
        assert_eq!(binding_from_cookie(Some(header)).as_deref(), Some("tok123"));
        assert_eq!(binding_from_cookie(Some("yonder_import_oauth=")), None);
        assert_eq!(binding_from_cookie(Some("other=1")), None);
        assert_eq!(binding_from_cookie(None), None);
    }

    #[test]
    fn expired_binding_cookie_clears_the_token() {
        let clear = expired_binding_cookie("https://yonder.example/cb");
        assert!(clear.starts_with("yonder_import_oauth=;"));
        assert!(clear.contains("Max-Age=0") && clear.contains("; Secure"));
        assert_eq!(binding_from_cookie(Some("yonder_import_oauth=")), None);
    }

    /// Private targets allowed so `http` schemes parse — the SSRF
    /// guard itself is exercised by the e2e suite.
    fn permissive_config() -> Config {
        let mut config = Config::default();
        config.federation.allow_private_targets = true;
        config
    }

    #[test]
    fn origin_accepts_url_domain_and_handle() {
        let config = permissive_config();
        for (input, want) in [
            ("https://old.example", "https://old.example"),
            (" https://old.example ", "https://old.example"),
            ("old.example", "https://old.example"),
            ("you@old.example", "https://old.example"),
            ("@you@old.example", "https://old.example"),
            ("@old.example", "https://old.example"),
            ("old.example:8443", "https://old.example:8443"),
            ("http://localhost:3000", "http://localhost:3000"),
        ] {
            assert_eq!(normalize_origin(input, &config).unwrap(), want, "{input}");
        }
    }

    #[test]
    fn origin_rejects_paths_queries_schemes_and_empty_hosts() {
        let config = permissive_config();
        for input in [
            "old.example/users/me",
            "https://old.example/users/me",
            "https://old.example?x=1",
            "https://old.example#frag",
            "https://user@old.example",
            "https://user:pw@old.example",
            "ftp://old.example",
            "",
            "@",
            "you@",
            "not a host",
        ] {
            assert!(normalize_origin(input, &config).is_err(), "{input}");
        }
    }

    #[test]
    fn scope_coverage_flags_only_what_grant_lacks() {
        let cats = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The umbrella read covers everything.
        assert!(scope_uncovered("read", &cats(&["statuses", "dms", "bookmarks"])).is_empty());
        // Granular equivalents satisfy their categories without `read`.
        assert!(scope_uncovered(
            "read:statuses read:bookmarks read:accounts",
            &cats(&["statuses", "dms", "bookmarks", "profile"])
        )
        .is_empty());
        // The paused-session shape: relationship scopes alone leave
        // the content categories uncovered.
        assert_eq!(
            scope_uncovered(
                "read:accounts read:blocks read:mutes",
                &cats(&[
                    "statuses",
                    "dms",
                    "bookmarks",
                    "favourites",
                    "following",
                    "observations",
                ])
            ),
            cats(&["statuses", "dms", "bookmarks", "favourites", "observations"])
        );
        // Unknown categories conservatively require `read`.
        assert_eq!(scope_uncovered("read:accounts", &cats(&["bogus"])), cats(&["bogus"]));
        assert!(scope_uncovered("read", &cats(&["bogus"])).is_empty());
    }

    #[test]
    fn return_to_accepts_only_local_paths() {
        for good in ["/settings/data", "/settings/migration", "/restricted"] {
            assert_eq!(validate_return_to(good).unwrap(), good, "{good}");
        }
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "/\tevil.example",
            "/path\nevil",
            "evil.example",
            "https://evil.example",
            "/path?x=1",
            "/path#frag",
            "",
            "/",
        ] {
            assert!(validate_return_to(bad).is_err(), "{bad:?}");
        }
    }
}
