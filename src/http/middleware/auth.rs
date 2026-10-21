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
    /// `users.id` owning the token.
    pub user_id: i64,
    /// `accounts.id` backing the user.
    pub account_id: i64,
    /// OAuth scopes granted to the token (space-separated).
    pub scopes: String,
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
        "SELECT t.user_id, u.account_id, t.scopes
         FROM oauth_access_tokens t
         JOIN users u ON u.id = t.user_id
         JOIN accounts a ON a.id = u.account_id
         WHERE t.token_hash = $1
           AND t.revoked_at IS NULL
           AND (t.expires_at IS NULL OR t.expires_at > now())
           AND a.suspended_at IS NULL",
    )
    .bind(token_hash(token))
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(Error::Unauthorized);
    };
    Ok(AuthContext {
        user_id: row.get("user_id"),
        account_id: row.get("account_id"),
        scopes: row.get("scopes"),
    })
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
