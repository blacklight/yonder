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

/// Hex-encoded SHA-256 of a bearer token — the `token_hash` format
/// written by epic 07's token issuance.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Resolve a bearer token to an [`AuthContext`], or `Unauthorized`
/// when unknown, revoked, or expired.
pub async fn resolve_token(state: &AppState, token: &str) -> Result<AuthContext> {
    let row = sqlx::query(
        "SELECT t.user_id, u.account_id, t.scopes
         FROM oauth_access_tokens t
         JOIN users u ON u.id = t.user_id
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
    Ok(AuthContext {
        user_id: row.get("user_id"),
        account_id: row.get("account_id"),
        scopes: row.get("scopes"),
    })
}

/// Pull the token off a request — `Authorization: Bearer` first, then
/// the `access_token` query parameter (WebSocket/streaming clients).
pub fn bearer_token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION) {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                return Some(token.to_string());
            }
        }
    }
    Query::<HashMap<String, String>>::try_from_uri(uri)
        .ok()
        .and_then(|Query(q)| q.get("access_token").cloned())
        .filter(|t| !t.is_empty())
}

impl FromRequestParts<Arc<AppState>> for AuthContext {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        // The rate limiter may already have resolved this request.
        if let Some(MaybeAuth(cached)) = parts.extensions.get::<MaybeAuth>() {
            return cached.clone().ok_or(Error::Unauthorized);
        }
        let token = bearer_token(&parts.headers, &parts.uri).ok_or(Error::Unauthorized)?;
        resolve_token(state, &token).await
    }
}

/// Optional authentication: `Some` when a valid token was presented,
/// `None` for anonymous requests. An invalid token is treated as
/// anonymous here — routes requiring auth use [`AuthContext`].
#[derive(Debug, Clone)]
pub struct MaybeAuth(pub Option<AuthContext>);

impl FromRequestParts<Arc<AppState>> for MaybeAuth {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> std::result::Result<Self, Self::Rejection> {
        if let Some(cached) = parts.extensions.get::<MaybeAuth>() {
            return Ok(cached.clone());
        }
        let auth = match bearer_token(&parts.headers, &parts.uri) {
            Some(token) => resolve_token(state, &token).await.ok(),
            None => None,
        };
        Ok(MaybeAuth(auth))
    }
}
