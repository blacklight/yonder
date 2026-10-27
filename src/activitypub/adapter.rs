//! The thin axum→`dispatch_multi` bridge.
//!
//! Pubby's `activitypub_router_multi` would process POST inboxes
//! inline (blocking `ureq` signature fetches on the runtime thread,
//! no durability) — so every GET goes through this adapter instead:
//! translate the axum request into a `DispatchRequest`, run
//! `dispatch_multi` inside `block_in_place` (pubby dispatch is sync and
//! may hit storage/HTTP), and translate the `HttpResp` back.
//!
//! The request translation mirrors pubby's own `make_request` —
//! `OriginalUri` supplies the path remote servers actually signed and
//! `ConnectInfo` supplies the default rate-limit bucket key.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, OriginalUri, Query, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use pubby_sdk::server::{dispatch_multi, DispatchOptions, DispatchRequest, HttpResp};

use crate::http::AppState;

use super::actors::sync_call;

/// Body cap matching pubby's `MAX_INBOX_BODY`.
const MAX_BODY: usize = 8 * 1024 * 1024;

pub(crate) fn headers_to_map(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn into_response(resp: HttpResp) -> Response {
    let mut builder = axum::http::Response::builder().status(resp.status);
    for (k, v) in &resp.headers {
        builder = builder.header(k, v);
    }
    builder
        .body(Body::from(resp.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Translate the axum `Request` into a `DispatchRequest` (mirrors
/// pubby's `make_request`).
async fn make_request(req: Request) -> std::result::Result<DispatchRequest, Box<Response>> {
    let (parts, body) = req.into_parts();
    let uri = parts
        .extensions
        .get::<OriginalUri>()
        .map(|o| o.0.clone())
        .unwrap_or(parts.uri);
    let path = uri.path().to_string();
    let query: HashMap<String, String> = Query::try_from_uri(&uri)
        .map(|Query(q)| q)
        .unwrap_or_default();
    let request_target = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let remote_addr = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_default();
    let headers = headers_to_map(&parts.headers);
    let body = to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Box::new(StatusCode::PAYLOAD_TOO_LARGE.into_response()))?;
    Ok(DispatchRequest {
        method: parts.method.as_str().to_string(),
        path,
        query,
        headers,
        body: body.to_vec(),
        remote_addr,
        request_target: Some(request_target),
    })
}

/// The AP GET fallback: `/.well-known/*`, `/nodeinfo/*` and every
/// `/ap/*` GET route land here and fan into `dispatch_multi`.
pub async fn serve(state: Arc<AppState>, req: Request) -> Response {
    let dreq = match make_request(req).await {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    dispatch(&state, &dreq)
}

/// Fan a [`DispatchRequest`] into `dispatch_multi` with the standard
/// options. Shared with `permalinks` — web permalinks route a
/// synthesized request on the canonical `/ap/*` path while
/// `request_target` keeps the signed web path for authorized-fetch.
pub(crate) fn dispatch(state: &Arc<AppState>, dreq: &DispatchRequest) -> Response {
    let Some(ap) = &state.ap else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let options = DispatchOptions {
        authorized_fetch: state.config().federation.authorized_fetch,
        document_cache: Some(ap.core.doc_cache.clone()),
        // Pubby's rate limiter stays off — it only ever sees GETs (the
        // POST inboxes are our own routes behind the named bucket).
        ..DispatchOptions::default()
    };
    let multi = ap.multi.clone();
    let resp = sync_call(move || dispatch_multi(&multi, dreq, "/ap", &options));
    into_response(resp)
}
