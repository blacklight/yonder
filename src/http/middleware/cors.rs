//! CORS — `Access-Control-Allow-Origin: *` on every response, and a
//! permissive preflight for every `OPTIONS` request.
//!
//! API auth is Bearer-token only — there are no cookies, so a
//! cross-origin page has no ambient authority to ride: it can only
//! make an authenticated call when it already holds a token, and
//! responses only ever contain data that token (or anonymity)
//! entitles it to. Gating paths would therefore protect nothing,
//! while breaking browser-based fediverse clients (columnly.social
//! and friends) that register apps, run OAuth, and call any
//! endpoint — including authenticated ones like
//! `accounts/verify_credentials` — directly.
//!
//! `http.cors_enabled`/`http.cors_origins` are deprecated: kept in
//! the config schema for compatibility but no longer consulted.

use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// `from_fn` entry point — see module docs.
pub async fn handle(req: Request<Body>, next: Next) -> Response {
    if req.method() == Method::OPTIONS {
        return preflight(&req);
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("link, x-request-id"),
    );
    res
}

/// Every `OPTIONS` is treated as a CORS preflight: reflect the
/// requested method and headers so any verb/header combination a
/// browser client needs passes. Bare probes (no
/// `Access-Control-Request-Method`) get the common method set.
fn preflight(req: &Request<Body>) -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    let methods = req
        .headers()
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|v| v.to_str().ok())
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS".to_string());
    if let Ok(v) = HeaderValue::from_str(&methods) {
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, v);
    }
    match req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        Some(requested) => {
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
        }
        None => {
            h.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("authorization, content-type, idempotency-key"),
            );
        }
    }
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("7200"),
    );
    h.append(
        header::VARY,
        HeaderValue::from_static("access-control-request-method"),
    );
    h.append(
        header::VARY,
        HeaderValue::from_static("access-control-request-headers"),
    );
    res.into_response()
}
