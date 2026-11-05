//! CORS — two policies in one middleware:
//!
//! - **Public reads** ([`is_public_path`]): `Access-Control-Allow-Origin:
//!   *` on GET/HEAD/OPTIONS. These endpoints serve anonymous callers
//!   anyway (`MaybeAuth` or no auth extractor), and API auth is
//!   Bearer-only — a cross-origin page can never attach a token, so
//!   `*` exposes nothing a plain GET wouldn't. This is what lets
//!   browser-based fediverse clients (columnly.social and friends)
//!   discover instances via nodeinfo/instance endpoints.
//! - **Configured origins** (`http.cors_enabled`/`http.cors_origins`):
//!   the pre-existing allow-list for every other path.
//!
//! App-level rather than nginx: the policy mirrors the route surface,
//! which only the application knows, and reverse-proxy setups vary
//! per deployment.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::http::AppState;

/// `from_fn_with_state` entry point — see module docs.
pub async fn handle(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let config = state.config();
    let http = &config.http;
    let path = req.uri().path();
    let method = req.method().clone();

    if is_public_path(path) && matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        if method == Method::OPTIONS {
            return public_preflight(&req).into_response();
        }
        let mut res = next.run(req).await;
        res.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        return res;
    }

    if !http.cors_enabled || http.cors_origins.is_empty() {
        return next.run(req).await;
    }

    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let Some(origin) = origin.filter(|o| http.cors_origins.iter().any(|a| a == o)) else {
        return next.run(req).await;
    };

    let is_preflight = method == Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let origin_value =
        HeaderValue::from_str(&origin).unwrap_or_else(|_| HeaderValue::from_static(""));

    if is_preflight {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization, content-type, idempotency-key"),
        );
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("7200"),
        );
        return res;
    }

    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("link, x-request-id"),
    );
    h.append(header::VARY, HeaderValue::from_static("origin"));
    res
}

/// Preflight for the public surface — echoes the requested headers so
/// any safelisted-plus-custom header combination passes.
fn public_preflight(req: &Request<Body>) -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    if let Some(requested) = req.headers().get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("7200"),
    );
    res
}

/// Anonymous-safe read paths — mirrors the `MaybeAuth`/no-auth GET
/// surface. Entity ids are numeric snowflakes, so the digit checks keep
/// lookalike static segments (`lookup`, `search`, `home`, `lists`)
/// out; method scoping keeps writes on the same paths covered.
fn is_public_path(path: &str) -> bool {
    const EXACT: &[&str] = &[
        "/.well-known/webfinger",
        "/.well-known/nodeinfo",
        "/.well-known/host-meta",
        "/.well-known/host-meta.json",
        "/api/v1/instance",
        "/api/v1/instance/extended_description",
        "/api/v1/instance/peers",
        "/api/v1/instance/domain_blocks",
        "/api/v1/instance/logo",
        "/api/v1/instance/banner",
        "/api/v2/instance",
        "/api/v1/custom_emojis",
        "/api/v1/accounts/lookup",
        "/api/v1/timelines/public",
        "/api/v1/trends",
        "/api/v1/trends/tags",
        "/api/v1/trends/statuses",
        "/api/v1/trends/links",
        "/api/v1/search",
        "/api/v2/search",
    ];
    if EXACT.contains(&path)
        || path.starts_with("/nodeinfo/")
        || path.starts_with("/media/")
        || path.starts_with("/users/")
        || path.starts_with("/tags/")
        || path.starts_with("/lists/")
    {
        return true;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match seg.as_slice() {
        // /api/v1/accounts/{id} and /api/v1/accounts/{id}/statuses
        ["api", "v1", "accounts", id] | ["api", "v1", "accounts", id, "statuses"] if digits(id) => {
            true
        }
        // /api/v1/statuses/{id} plus its public sub-documents
        ["api", "v1", "statuses", id] if digits(id) => true,
        ["api", "v1", "statuses", id, sub]
            if digits(id)
                && matches!(
                    *sub,
                    "context" | "quotes" | "favourited_by" | "reblogged_by"
                ) =>
        {
            true
        }
        // /api/v1/pleroma/statuses/{id}/reactions
        ["api", "v1", "pleroma", "statuses", id, "reactions"] if digits(id) => true,
        // /api/v1/tags/{name}, /api/v1/timelines/tag/{tag}, /api/v1/polls/{id}
        ["api", "v1", "tags", _] => true,
        ["api", "v1", "timelines", "tag", _] => true,
        ["api", "v1", "polls", id] if digits(id) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_cover_discovery_and_reads() {
        for path in [
            "/.well-known/webfinger",
            "/.well-known/nodeinfo",
            "/.well-known/host-meta",
            "/nodeinfo/2.1",
            "/api/v1/instance",
            "/api/v1/instance/peers",
            "/api/v2/instance",
            "/api/v1/accounts/lookup",
            "/api/v1/accounts/12345",
            "/api/v1/accounts/12345/statuses",
            "/api/v1/statuses/987",
            "/api/v1/statuses/987/context",
            "/api/v1/statuses/987/quotes",
            "/api/v1/pleroma/statuses/987/reactions",
            "/api/v1/timelines/public",
            "/api/v1/timelines/tag/rust",
            "/api/v1/tags/rust",
            "/api/v1/trends/statuses",
            "/api/v1/search",
            "/api/v1/polls/42",
            "/api/v1/custom_emojis",
            "/media/abc/img.png",
            "/users/alice.atom",
        ] {
            assert!(is_public_path(path), "{path} should be public");
        }
    }

    #[test]
    fn private_lookalikes_stay_out() {
        for path in [
            "/api/v1/accounts/search", // auth-required account search
            "/api/v1/accounts/verify_credentials",
            "/api/v1/accounts/12345/lists", // auth-required
            "/api/v1/accounts/12345/featured_tags",
            "/api/v1/statuses",             // POST list_by_ids is auth'd
            "/api/v1/statuses/987/history", // auth-required edit history
            "/api/v1/statuses/987/source",
            "/api/v1/statuses/987/favourite",
            "/api/v1/timelines/home",
            "/api/v1/timelines/list/7",
            "/api/v1/notifications",
            "/api/v1/announcements",
            "/api/v1/yonder/settings",
            "/api/v1/admin/reports",
            "/api/v2/suggestions",
            "/api/v2/media/1",
            "/healthz",
        ] {
            assert!(!is_public_path(path), "{path} must not be public");
        }
    }
}
