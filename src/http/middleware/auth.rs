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
/// when unknown, revoked, expired, or belonging to a suspended account.
pub async fn resolve_token(state: &AppState, token: &str) -> Result<AuthContext> {
    let row = sqlx::query(
        "SELECT t.id AS token_id, t.user_id, u.account_id, t.scopes, \
                a.role, s.id AS session_id, \
                u.approved_at IS NOT NULL AS approved, \
                u.email_verified_at IS NOT NULL AS email_verified
         FROM oauth_access_tokens t
         JOIN users u ON u.id = t.user_id
         JOIN accounts a ON a.id = u.account_id
         LEFT JOIN sessions s ON s.access_token_id = t.id
         WHERE t.token_hash = $1
           AND t.revoked_at IS NULL
           AND (t.expires_at IS NULL OR t.expires_at > now())
           AND a.suspended_at IS NULL
           AND a.disabled_at IS NULL",
    )
    .bind(token_hash(token))
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(Error::Unauthorized);
    };
    let auth = AuthContext {
        token_id: row.get("token_id"),
        user_id: row.get("user_id"),
        account_id: row.get("account_id"),
        scopes: row.get("scopes"),
        role: row.get("role"),
        session_id: row.get("session_id"),
        approved: row.get("approved"),
        email_verified: row.get("email_verified"),
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
            TokenResolution::Authed(auth) => Ok(auth),
            TokenResolution::Anonymous | TokenResolution::Rejected => Err(Error::Unauthorized),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}

/// Optional authentication: `Some` when a valid token was presented,
/// `None` for anonymous requests. An invalid token is anonymous; an
/// infrastructure failure during resolution rejects the request with
/// a 5xx rather than silently downgrading it.
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
            TokenResolution::Authed(auth) => Ok(MaybeAuth(Some(auth))),
            TokenResolution::Anonymous | TokenResolution::Rejected => Ok(MaybeAuth(None)),
            TokenResolution::Failed(e) => Err(Error::Internal(e.to_string())),
        }
    }
}
