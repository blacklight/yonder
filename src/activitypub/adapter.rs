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
    http::{header, StatusCode},
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
    let anonymous = super::edge::anonymous(req.headers());
    let dreq = match make_request(req).await {
        Ok(r) => r,
        Err(resp) => return *resp,
    };
    // dispatch_multi only serves public documents — private objects
    // go through `posts::serve_named_object`.
    // Actor paths get their document extended post-dispatch
    // (`inject_actor_extensions`), so pubby's doc-cache ETag — computed
    // over the un-extended body — must not drive a 304 here: strip
    // `If-None-Match` and let the injected response carry its own
    // validator.
    let is_actor_path = is_actor_doc_path(&dreq.path);
    let mut dreq = dreq;
    if is_actor_path {
        dreq.headers.remove("if-none-match");
    }
    let resp = dispatch(&state, &dreq).await;
    let resp = if is_actor_path {
        inject_actor_extensions(&dreq.path, resp).await
    } else {
        resp
    };
    super::edge::apply(resp, true, anonymous)
}

/// The exact actor-document path (`/ap/users/{username}`, no further
/// segments) — the only document that gets FEP-8a8e extensions.
fn is_actor_doc_path(path: &str) -> bool {
    path.strip_prefix("/ap/users/")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// FEP-8a8e §16: advertise the upcoming-events collection on the
/// actor document. Pubby's `ActorConfig` has no field for it, so the
/// property is injected here — after pubby's dispatch (its doc-cache
/// keeps storing the un-extended document), and only on the exact
/// actor path. Best-effort: any parse hiccup returns the original
/// response.
async fn inject_actor_extensions(path: &str, resp: Response) -> Response {
    if !is_actor_doc_path(path) {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    if parts.status != StatusCode::OK {
        return Response::from_parts(parts, body);
    }
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("json"));
    if !is_json {
        return Response::from_parts(parts, body);
    }
    let bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        // Not cacheable — the edge wrapper must not freeze a body-read
        // failure.
        Err(_) => {
            return super::edge::apply(
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                false,
                true,
            )
        }
    };
    let Ok(mut doc) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let mut injected = false;
    if let Some(actor_id) = doc.get("id").and_then(|v| v.as_str()) {
        doc["upcomingEvents"] =
            serde_json::json!(format!("{actor_id}/collections/upcoming_events"));
        // Pubby's AP context doesn't carry the FEP-8a8e terms — swap
        // in the event context so `upcomingEvents` resolves for
        // JSON-LD consumers. AS2 terms stay untouched.
        doc["@context"] = super::context::event_context();
        injected = true;
    }
    if !injected {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let json_bytes = serde_json::to_vec(&doc).unwrap_or_else(|_| bytes.to_vec());
    // The body changed, so pubby's ETag (computed over the cached,
    // un-extended body) no longer describes it — recompute over the
    // final bytes, matching pubby's SHA-1 hex format.
    use sha1::{Digest as _, Sha1};
    let etag = format!("{:x}", Sha1::digest(&json_bytes));
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!("\"{etag}\"")) {
        parts.headers.insert(header::ETAG, v);
    }
    Response::from_parts(parts, Body::from(json_bytes))
}

/// Fan a [`DispatchRequest`] into `dispatch_multi` with the standard
/// options. Shared with `permalinks` — web permalinks route a
/// synthesized request on the canonical `/ap/*` path while
/// `request_target` keeps the signed web path for authorized-fetch.
pub(crate) async fn dispatch(state: &Arc<AppState>, dreq: &DispatchRequest) -> Response {
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
    let dreq = dreq.clone();
    let resp = sync_call(move || dispatch_multi(&multi, &dreq, "/ap", &options)).await;
    into_response(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn actor_response(body: &str, etag: Option<&'static str>) -> Response {
        let mut resp = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/activity+json")],
            Body::from(body.to_string()),
        )
            .into_response();
        if let Some(etag) = etag {
            resp.headers_mut()
                .insert(header::ETAG, HeaderValue::from_static(etag));
        }
        resp
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        to_bytes(resp.into_body(), MAX_BODY).await.unwrap().to_vec()
    }

    #[tokio::test]
    async fn injects_upcoming_events_and_recomputes_etag() {
        let doc = r#"{"id":"https://x.test/ap/users/alice","type":"Person"}"#;
        // Pubby's ETag describes the un-extended body.
        let resp = actor_response(doc, Some("\"deadbeef\""));
        let resp = inject_actor_extensions("/ap/users/alice", resp).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let bytes = body_bytes(resp).await;
        let out: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            out["upcomingEvents"],
            "https://x.test/ap/users/alice/collections/upcoming_events"
        );
        // The context is pubby's AP array with the FEP terms merged
        // into its object member.
        assert!(out["@context"].is_array());
        // The validator now matches the served body (SHA-1 hex,
        // quoted), not pubby's pre-injection value.
        use sha1::{Digest as _, Sha1};
        assert_eq!(etag, format!("\"{:x}\"", Sha1::digest(&bytes)));
        assert_ne!(etag, "\"deadbeef\"");
    }

    #[tokio::test]
    async fn non_actor_paths_and_non_json_pass_through() {
        let doc = r#"{"id":"https://x.test/ap/users/alice"}"#;
        let resp = actor_response(doc, None);
        let resp = inject_actor_extensions("/ap/users/alice/collections/featured", resp).await;
        let bytes = body_bytes(resp).await;
        assert_eq!(bytes, doc.as_bytes());

        let resp = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            Body::from("<html/>"),
        )
            .into_response();
        let resp = inject_actor_extensions("/ap/users/alice", resp).await;
        let bytes = body_bytes(resp).await;
        assert_eq!(bytes, b"<html/>");
    }

    #[tokio::test]
    async fn non_ok_status_passes_through() {
        let resp = StatusCode::NOT_FOUND.into_response();
        let resp = inject_actor_extensions("/ap/users/alice", resp).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
