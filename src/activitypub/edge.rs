//! Edge-cache contract for AP dereference responses.
//!
//! `docker/nginx.conf` edge-caches anonymous AP-JSON fetches in the
//! `yonder_fedoc` zone (single-flighted misses via `proxy_cache_lock`).
//! The app bounds freshness from its side: cacheable answers carry
//! `X-Accel-Expires` plus a matching public `Cache-Control`, and
//! answers that must never be stored — private (signer-gated)
//! documents, non-document statuses and any response to a
//! credential-bearing request — carry `X-Accel-Expires: 0` +
//! `Cache-Control: no-store` so the proxy's `proxy_cache_valid`
//! fallback can never keep a copy.
//!
//! An HTTP `Signature` header does not make a request credentialed
//! here — a public document is public regardless of who asked — but
//! nginx additionally bypasses the cache for signed requests so a
//! cached anonymous 404 never hides a document a signer can see.

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
};

/// Freshness the app grants the edge cache, in seconds — mirrored by
/// `proxy_cache_valid` in `docker/nginx.conf` (the fallback for
/// responses without `X-Accel-Expires`). A follow-up could compute a
/// per-document TTL from the doc-cache's remaining freshness; the
/// [`apply`] signature already takes the flags a computed TTL would
/// need, so only this constant would change shape.
pub(crate) const EDGE_TTL_SECS: u32 = 15;

/// Whether the request carried no `Authorization` header and no
/// cookies — the same credential check `docker/nginx.conf`'s
/// `yonder_no_cache` map applies.
pub(crate) fn anonymous(headers: &HeaderMap) -> bool {
    !headers.contains_key(header::AUTHORIZATION) && !headers.contains_key(header::COOKIE)
}

/// Apply the edge-cache contract to a dereference response.
///
/// `public` — the answer is safe for anonymous readers (status
/// `public`/`unlisted` objects, actor/collection/well-known
/// documents). `anonymous` — [`anonymous`] on the request headers.
///
/// Cacheable: anonymous 200s with a machine-readable content type and
/// anonymous 404s (a signer bypasses the edge cache anyway, so a
/// cached miss can't hide a private document). Everything else —
/// private documents, credential-bearing requests, redirects, errors —
/// is explicitly uncacheable.
pub(crate) fn apply(mut resp: Response, public: bool, anonymous: bool) -> Response {
    let cacheable = public
        && anonymous
        && match resp.status() {
            StatusCode::OK => machine_readable(&resp),
            StatusCode::NOT_FOUND => true,
            _ => false,
        };
    let h = resp.headers_mut();
    if cacheable {
        h.insert(
            "x-accel-expires",
            HeaderValue::from_str(&EDGE_TTL_SECS.to_string()).unwrap(),
        );
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_str(&format!("public, max-age={EDGE_TTL_SECS}")).unwrap(),
        );
    } else {
        h.insert("x-accel-expires", HeaderValue::from_static("0"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    resp
}

/// Whether the response body is a machine-readable document (AP-JSON,
/// JRD, XRD) — the only bodies the edge cache is meant to store.
fn machine_readable(resp: &Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.to_ascii_lowercase();
            ct.contains("json") || ct.contains("xml")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::header::HeaderName};

    fn resp(status: StatusCode, content_type: Option<&'static str>) -> Response {
        let mut r = Response::builder().status(status);
        if let Some(ct) = content_type {
            r = r.header(header::CONTENT_TYPE, ct);
        }
        r.body(Body::empty()).unwrap()
    }

    fn credentialed() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
        h
    }

    #[test]
    fn public_anonymous_json_is_cacheable() {
        let r = apply(
            resp(
                StatusCode::OK,
                Some("application/activity+json; charset=utf-8"),
            ),
            true,
            true,
        );
        assert_eq!(r.headers()["x-accel-expires"], "15");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "public, max-age=15");
    }

    #[test]
    fn anonymous_not_found_is_cacheable() {
        let r = apply(resp(StatusCode::NOT_FOUND, None), true, true);
        assert_eq!(r.headers()["x-accel-expires"], "15");
    }

    #[test]
    fn private_document_is_never_cached() {
        let r = apply(
            resp(StatusCode::OK, Some("application/activity+json")),
            false,
            true,
        );
        assert_eq!(r.headers()["x-accel-expires"], "0");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[test]
    fn credentialed_request_is_never_cached() {
        let r = apply(
            resp(StatusCode::OK, Some("application/activity+json")),
            true,
            anonymous(&credentialed()),
        );
        assert_eq!(r.headers()["x-accel-expires"], "0");
    }

    #[test]
    fn html_and_errors_are_never_cached() {
        for r in [
            apply(resp(StatusCode::OK, Some("text/html")), true, true),
            apply(resp(StatusCode::INTERNAL_SERVER_ERROR, None), true, true),
            apply(resp(StatusCode::OK, None), true, true),
        ] {
            assert_eq!(r.headers()["x-accel-expires"], "0");
        }
    }

    #[test]
    fn anonymous_gates_on_credentials() {
        let mut cookies = HeaderMap::new();
        cookies.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=1"),
        );
        assert!(!anonymous(&cookies));
        assert!(anonymous(&HeaderMap::new()));
    }
}
