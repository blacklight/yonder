//! Authentication extractor.
//!
//! [`AuthContext`] is an axum extractor: it resolves a bearer token
//! (`Authorization: Bearer <token>` or `?access_token=` for streaming
//! clients that can't set headers) against `oauth_access_tokens` and
//! rejects unauthenticated requests with 401. [`MaybeAuth`] is the
//! non-rejecting sibling for public routes and rate-limit identity.
//!
//! Token lookup is real — `token_hash = hex(sha256(token))`, matching
//! what epic 07's issuance writes — but the resolver sits behind
//! [`resolve_token`] so epic 07 can extend it (scope checks, session
//! cookies) without touching call sites.
//!
//! Error semantics matter to Mastodon clients, which treat 401 as "log
//! me out": a missing/invalid/revoked token is [`TokenResolution::Anonymous`],
//! while an infrastructure failure (database down) is
//! [`TokenResolution::Failed`] and surfaces as 500, never as 401.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Query},
    http::{header::AUTHORIZATION, request::Parts, HeaderMap, Uri},
};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{http::AppState, Error, Result};

/// The credential class a token belongs to: ordinary full-access
/// tokens, or `restricted` export-only sessions issued to suspended
/// accounts (epic 31 restricted access).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessClass {
    Ordinary,
    Restricted,
}

impl AccessClass {
    fn from_wire(s: &str) -> AccessClass {
        match s {
            "restricted" => AccessClass::Restricted,
            _ => AccessClass::Ordinary,
        }
    }
}

/// The authenticated principal attached to a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// `oauth_access_tokens.id` backing this request — lets mutations
    /// (e.g. password change) spare the caller's own token while
    /// revoking the rest.
    pub token_id: i64,
    /// `users.id` owning the token.
    pub user_id: i64,
    /// `accounts.id` backing the user.
    pub account_id: i64,
    /// OAuth scopes granted to the token (space-separated).
    pub scopes: String,
    /// `accounts.role` (`user`/`moderator`/`admin`).
    pub role: String,
    /// Linked `sessions.id` when the token wraps an interactive login.
    pub session_id: Option<i64>,
    /// `users.approved_at IS NOT NULL` — the approval gate for writes.
    pub approved: bool,
    /// `users.email_verified_at IS NOT NULL`.
    pub email_verified: bool,
    /// Ordinary tokens exercise their scopes; restricted tokens only
    /// reach the export-only surface ([`RestrictedAuth`]/[`ExportAuth`]).
    pub access_class: AccessClass,
}

/// Cached outcome of resolving a request's token — stored in the
/// request extensions by the rate limiter so downstream extractors
/// don't hit the database twice.
#[derive(Debug, Clone)]
pub enum TokenResolution {
    /// A valid token resolved to a principal.
    Authed(AuthContext),
    /// No token was presented.
    Anonymous,
    /// A token was presented but is unknown/revoked/expired/suspended.
    /// Extractors for protected routes answer 401; optional-auth
    /// routes treat this as anonymous.
    Rejected,
    /// Resolution failed on infrastructure (database/redis). Surfaced
    /// as a 5xx — never reported to the client as an auth failure.
    Failed(Arc<Error>),
}

impl TokenResolution {
    /// The resolved principal, if any.
    pub fn auth(&self) -> Option<&AuthContext> {
        match self {
            TokenResolution::Authed(auth) => Some(auth),
            _ => None,
        }
    }
}

/// Hex-encoded SHA-256 of a bearer token — the `token_hash` format
/// written by epic 07's token issuance.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Resolve a bearer token to an [`AuthContext`], or `Unauthorized`
/// when unknown, revoked, expired, or mismatched with the account's
/// state. Ordinary tokens require a live account (unsuspended,
/// enabled, not deleted); restricted tokens resolve only while the
/// account is suspended — an unsuspension kills them, forcing a fresh
/// normal login — and never for accounts pending deletion.
pub async fn resolve_token(state: &AppState, token: &str) -> Result<AuthContext> {
    let row = sqlx::query(
        "SELECT t.id AS token_id, t.user_id, u.account_id, t.scopes, \
                t.access_class, a.role, s.id AS session_id, \
                u.approved_at IS NOT NULL AS approved, \
                u.email_verified_at IS NOT NULL AS email_verified, \
                a.suspended_at IS NOT NULL AS suspended, \
                a.disabled_at IS NOT NULL OR a.deleted_at IS NOT NULL AS gone, \
                EXISTS(SELECT 1 FROM account_deletions d \
                       WHERE d.account_id = a.id AND d.state IN ('pending','running')) \
                    AS deleting
         FROM oauth_access_tokens t
         JOIN users u ON u.id = t.user_id
         JOIN accounts a ON a.id = u.account_id
         LEFT JOIN sessions s ON s.access_token_id = t.id
         WHERE t.token_hash = $1
           AND t.revoked_at IS NULL
           AND (t.expires_at IS NULL OR t.expires_at > now())",
    )
    .bind(token_hash(token))
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(Error::Unauthorized);
    };
    let access_class = AccessClass::from_wire(row.get("access_class"));
    let suspended: bool = row.get("suspended");
    let gone: bool = row.get("gone");
    let deleting: bool = row.get("deleting");
    let usable = match access_class {
        AccessClass::Ordinary => !suspended && !gone,
        AccessClass::Restricted => suspended && !gone && !deleting,
    };
    if !usable {
        return Err(Error::Unauthorized);
    }
    let auth = AuthContext {
        token_id: row.get("token_id"),
        user_id: row.get("user_id"),
        account_id: row.get("account_id"),
        scopes: row.get("scopes"),
        role: row.get("role"),
        session_id: row.get("session_id"),
        approved: row.get("approved"),
        email_verified: row.get("email_verified"),
        access_class,
    };
    if let Some(session_id) = auth.session_id {
        touch_session(state, session_id).await;
    }
    Ok(auth)
}

/// Bump `sessions.last_activity_at` at most once per minute per session
/// (a Redis `SET NX EX` marker gates the Postgres write). A Redis
/// outage must never invalidate a valid bearer token, so failures are
/// logged and swallowed.
async fn touch_session(state: &AppState, session_id: i64) {
    let outcome: Result<()> = async {
        let mut conn = state.redis.conn().await?;
        let key = state.redis.key(&format!("session-touch:{session_id}"));
        let acquired: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(60)
            .query_async(&mut *conn)
            .await?;
        if acquired.is_none() {
            return Ok(());
        }
        sqlx::query("UPDATE sessions SET last_activity_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&state.db)
            .await?;
        Ok(())
    }
    .await;
    if let Err(e) = outcome {
        tracing::warn!(session_id, error = %e, "session activity touch failed");
    }
}

/// Pull the token off a request — `Authorization: Bearer` first (the
/// scheme is case-insensitive per RFC 7235), then the `access_token`
/// query parameter (WebSocket/streaming clients).
pub fn bearer_token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some((scheme, token)) = value.split_once(' ') {
            if scheme.eq_ignore_ascii_case("bearer") {
                return Some(token.to_string());
            }
        }
    }
    Query::<HashMap<String, String>>::try_from_uri(uri)
        .ok()
        .and_then(|Query(q)| q.get("access_token").cloned())
        .filter(|t| !t.is_empty())
}

/// Resolve the request's token to a [`TokenResolution`], reusing the
/// extension cached by the rate limiter when present.
pub async fn resolve_request(state: &AppState, headers: &HeaderMap, uri: &Uri) -> TokenResolution {
    match bearer_token(headers, uri) {
        Some(token) => match resolve_token(state, &token).await {
            Ok(auth) => TokenResolution::Authed(auth),
            Err(Error::Unauthorized) => TokenResolution::Rejected,
            Err(e) => TokenResolution::Failed(Arc::new(e)),
        },
        None => TokenResolution::Anonymous,
    }
}

impl FromRequestParts<Arc<AppState>> for AuthContext {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        // The rate limiter may already have resolved this request.
        let resolution = match parts.extensions.get::<TokenResolution>() {
            Some(cached) => cached.clone(),
            None => resolve_request(state, &parts.headers, &parts.uri).await,
        };
        match resolution {
            // Restricted credentials resolve but never act as an
            // ordinary principal — 403, not 401, so clients know the
            // token is real yet out of scope.
            TokenResolution::Authed(auth) if auth.access_class == AccessClass::Restricted => Err(
                Error::Forbidden("This token is restricted to account exports"),
            ),
            TokenResolution::Authed(auth) => Ok(auth),
            TokenResolution::Anonymous | TokenResolution::Rejected => Err(Error::Unauthorized),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}

/// Extractor for the suspended-account export surface: accepts **only**
/// restricted-class tokens (issued by the password grant while the
/// account is suspended). Ordinary principals get 403 — the restricted
/// endpoints expose suspension metadata normal sessions should not
/// rely on.
#[derive(Debug, Clone)]
pub struct RestrictedAuth(pub AuthContext);

impl FromRequestParts<Arc<AppState>> for RestrictedAuth {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let resolution = match parts.extensions.get::<TokenResolution>() {
            Some(cached) => cached.clone(),
            None => resolve_request(state, &parts.headers, &parts.uri).await,
        };
        match resolution {
            TokenResolution::Authed(auth) if auth.access_class == AccessClass::Restricted => {
                Ok(RestrictedAuth(auth))
            }
            TokenResolution::Authed(_) => Err(Error::Forbidden(
                "This endpoint requires a restricted session",
            )),
            TokenResolution::Anonymous | TokenResolution::Rejected => Err(Error::Unauthorized),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}

/// Extractor for the export surface shared by ordinary and restricted
/// sessions: `POST/GET /api/v1/yonder/exports*` admit either class —
/// ordinary callers still pass the normal scope gate, restricted
/// callers exercise their implicit export capability.
#[derive(Debug, Clone)]
pub struct ExportAuth(pub AuthContext);

impl ExportAuth {
    /// The scope gate for ordinary callers; restricted sessions carry
    /// their capability in the class itself and skip it.
    pub fn require_scope(&self, config: &crate::config::Config, required: &str) -> Result<()> {
        match self.0.access_class {
            AccessClass::Ordinary => crate::auth::scopes::require_scope(config, &self.0, required),
            AccessClass::Restricted => Ok(()),
        }
    }
}

impl FromRequestParts<Arc<AppState>> for ExportAuth {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let resolution = match parts.extensions.get::<TokenResolution>() {
            Some(cached) => cached.clone(),
            None => resolve_request(state, &parts.headers, &parts.uri).await,
        };
        match resolution {
            TokenResolution::Authed(auth) => Ok(ExportAuth(auth)),
            TokenResolution::Anonymous | TokenResolution::Rejected => Err(Error::Unauthorized),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}

/// Optional authentication: `Some` when a valid token was presented,
/// `None` for anonymous requests. An invalid token is anonymous; an
/// infrastructure failure during resolution rejects the request with
/// a 5xx rather than silently downgrading it. Restricted tokens are
/// anonymous here: they hold no read capability outside the export
/// surface.
#[derive(Debug, Clone)]
pub struct MaybeAuth(pub Option<AuthContext>);

impl FromRequestParts<Arc<AppState>> for MaybeAuth {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        let resolution = match parts.extensions.get::<TokenResolution>() {
            Some(cached) => cached.clone(),
            None => resolve_request(state, &parts.headers, &parts.uri).await,
        };
        match resolution {
            TokenResolution::Authed(auth) if auth.access_class == AccessClass::Restricted => {
                Ok(MaybeAuth(None))
            }
            TokenResolution::Authed(auth) => Ok(MaybeAuth(Some(auth))),
            TokenResolution::Anonymous | TokenResolution::Rejected => Ok(MaybeAuth(None)),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}
