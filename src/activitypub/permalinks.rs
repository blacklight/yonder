//! Web permalinks → ActivityPub documents.
//!
//! Remote instances resolve a URL search with a signed `GET` carrying
//! `Accept: application/activity+json`. The public URL shapes —
//! `statuses.url` (`/@{acct}/{id}`), `accounts.url` (`/@{acct}`) and
//! the SPA's own `/statuses/{id}` — are unmatched routes that used to
//! land on the SPA fallback: a 200 `text/html` response a fetcher can
//! never parse, so the lookup silently returned nothing.
//!
//! [`crate::http::frontend::fallback`] calls [`resolve`] whenever the
//! `Accept` header prefers an AP representation (see
//! [`crate::http::negotiate`]); a resolved path answers with the
//! document, an unresolved one with a JSON 404, and non-AP accepts keep
//! the SPA behaviour.
//!
//! The status-object branch reuses [`posts::serve_named_object`], so
//! visibility scoping, HTTP-signature checks and the doc cache apply
//! unchanged — the signature target is the web path itself, which is
//! exactly what the remote signed. The actor branch synthesizes a
//! [`DispatchRequest`] routed as the canonical `/ap/users/{u}` path
//! while `request_target` keeps the signed web path for the
//! authorized-fetch check.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::Query,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use pubby_sdk::server::DispatchRequest;

use crate::http::{negotiate, AppState};

use super::{actors::actor_path, adapter, federation_base_url, posts};

/// Whether the `Accept` header asks for an ActivityPub representation.
/// Plain `application/json` counts — a remote fetcher that accepts only
/// JSON still wants the document, never the SPA shell.
pub fn wants_document(headers: &HeaderMap) -> bool {
    matches!(
        negotiate::accept(headers),
        negotiate::ContentKind::ActivityJson
            | negotiate::ContentKind::LdJson
            | negotiate::ContentKind::Json
    )
}

/// Resolve a web path to its ActivityPub document. `None` means the
/// path isn't a known permalink shape — the caller decides the answer
/// (a JSON 404 for AP accepts).
pub async fn resolve(
    state: &Arc<AppState>,
    uri: &Uri,
    headers: &HeaderMap,
    remote_addr: Option<SocketAddr>,
) -> Option<Response> {
    let segments: Vec<&str> = uri
        .path()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        // `/statuses/{id}` — the SPA route; `/@{acct}/{id}` — the
        // stored `statuses.url`. The acct segment is decorative, like
        // Mastodon's permalink: the id is authoritative.
        ["statuses", id] => Some(status(state, uri, headers, id).await),
        [acct, id] if acct.starts_with('@') => Some(status(state, uri, headers, id).await),
        // `/@{acct}` — the stored `accounts.url` profile permalink.
        [acct] if acct.starts_with('@') => {
            Some(actor(state, uri, headers, remote_addr, acct).await)
        }
        _ => None,
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(crate::error::ErrorBody {
            error: "not found".to_string(),
        }),
    )
        .into_response()
}

/// `/{…}/{id}` shapes → the status object. The author's username is
/// resolved for the signature-handler lookup (the path's acct is
/// decorative and needn't match).
async fn status(state: &Arc<AppState>, uri: &Uri, headers: &HeaderMap, id: &str) -> Response {
    let Ok(status_id) = id.parse::<i64>() else {
        return not_found();
    };
    let username: Option<String> = match sqlx::query_scalar(
        "SELECT a.username FROM statuses s \
         JOIN accounts a ON a.id = s.account_id WHERE s.id = $1",
    )
    .bind(status_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, status_id, "permalink status lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    match username {
        Some(username) => {
            posts::serve_named_object(state, &username, status_id, headers, uri).await
        }
        None => not_found(),
    }
}

/// `/@{acct}` → the actor document via `dispatch_multi` on the
/// canonical actor path. `request_target` stays the signed web path so
/// `authorized_fetch` verifies what the remote actually signed;
/// `remote_addr` keeps the request attributable.
async fn actor(
    state: &Arc<AppState>,
    uri: &Uri,
    headers: &HeaderMap,
    remote_addr: Option<SocketAddr>,
    acct: &str,
) -> Response {
    if state.ap.is_none() {
        return not_found();
    }
    let acct = &acct[1..];
    let (username, domain) = match acct.split_once('@') {
        Some((u, d)) => (u, Some(d)),
        None => (acct, None),
    };
    if username.is_empty() || username.contains('%') || domain.is_some_and(|d| d.is_empty()) {
        return not_found();
    }
    // `/@user@domain` is only ours when `domain` names this instance —
    // otherwise the document lives elsewhere and we claim nothing.
    if let Some(domain) = domain {
        if !local_domain(state, domain) {
            return not_found();
        }
    }
    let request_target = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let dreq = DispatchRequest {
        method: "GET".to_string(),
        path: actor_path(username),
        query: Query::try_from_uri(uri)
            .map(|Query(q)| q)
            .unwrap_or_default(),
        headers: adapter::headers_to_map(headers),
        body: Vec::new(),
        remote_addr: remote_addr.map(|a| a.ip().to_string()).unwrap_or_default(),
        request_target: Some(request_target),
    };
    adapter::dispatch(state, &dreq)
}

/// Whether `domain` names this instance — the WebFinger domain or the
/// federation base URL's host (dev setups where the base carries a
/// scheme/port while `instance.domain` is the bare name).
fn local_domain(state: &Arc<AppState>, domain: &str) -> bool {
    let domain = domain.to_lowercase();
    if let Some(ap) = &state.ap {
        if ap.multi.webfinger_domain.eq_ignore_ascii_case(&domain) {
            return true;
        }
    }
    url::Url::parse(&federation_base_url(&state.config()))
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .is_some_and(|host| host == domain)
}
