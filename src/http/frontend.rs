//! SPA serving (epic 23): the active frontend's static files as the
//! router **fallback** — axum only sends requests no defined route
//! claimed, so `/api/*`, ActivityPub, feeds, `/media/*`, OAuth,
//! `/openapi.json` and `/swagger-ui` always win by construction.
//!
//! Files resolve under the active frontend's root (see
//! [`crate::frontends::resolve_active`]); unmatched paths get
//! `index.html` back for client-side routing. When the builtin
//! frontend has not been built yet a small embedded placeholder
//! page answers instead, so the server always has a landing page.

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};

use crate::frontends::{self, Frontend};
use crate::http::AppState;

/// Minimal landing page served while `frontend/dist` does not exist —
/// keeps `cargo run` usable without the Node toolchain.
const PLACEHOLDER: &str = include_str!("placeholder.html");

/// Cache policy for hashed build artifacts under `/assets/`.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `GET`/`HEAD` fallback handler. Registration lives in
/// [`crate::http::build_router`] — the fallback must sit on the
/// `Router<Arc<AppState>>` before `.with_state` for the `State`
/// extractor to resolve.
pub async fn fallback(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    parts: axum::http::request::Parts,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    // Protocol paths must never fall through to the SPA: an unmatched
    // `/api/…` or `/.well-known/…` is a JSON 404, not `index.html` —
    // Mastodon clients parse the body and would choke on HTML.
    if is_api_path(uri.path()) {
        return (
            StatusCode::NOT_FOUND,
            Json(crate::error::ErrorBody {
                error: "not found".to_string(),
            }),
        )
            .into_response();
    }

    // Federation dereference: a remote instance resolving one of our
    // web permalinks asks for `application/activity+json` — answer
    // with the document, not the SPA shell (a 200 HTML response is a
    // silent miss in remote search). Unresolved paths and every
    // non-AP accept keep the file/entry behaviour below.
    if crate::activitypub::permalinks::wants_document(&headers) {
        let not_found = || {
            (
                StatusCode::NOT_FOUND,
                Json(crate::error::ErrorBody {
                    error: "not found".to_string(),
                }),
            )
                .into_response()
        };
        return match crate::activitypub::permalinks::resolve(
            &state,
            &uri,
            &headers,
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0),
        )
        .await
        {
            Some(response) => response,
            None => not_found(),
        };
    }

    let config = state.config();
    let frontend = frontends::resolve_active(&config, None);
    let Some(rel) = safe_path(uri.path()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    // `/` and every unmatched path fall through to the SPA entry.
    let response = if rel.as_os_str().is_empty() {
        serve_entry(&frontend)
    } else {
        serve_file(&frontend, &rel).unwrap_or_else(|| serve_entry(&frontend))
    };
    tag(&frontend.name, response)
}

/// Prefixes owned by protocol routes (API, federation, OAuth, media,
/// API docs). Defined routes never reach the fallback; what does is
/// genuinely unknown and must answer like the protocol it looks like.
fn is_api_path(path: &str) -> bool {
    path == "/openapi.json"
        || ["/api", "/.well-known", "/oauth", "/media", "/swagger-ui"]
            .iter()
            .any(|p| {
                path.strip_prefix(p)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
}

/// Stamp `x-yonder-frontend` on every frontend response — the
/// built-in service worker compares it with its own registration and
/// unregisters when an admin switches frontends, so a stale worker
/// cannot keep serving the old shell.
fn tag(name: &str, mut response: Response) -> Response {
    if let Ok(value) = HeaderValue::from_str(name) {
        response.headers_mut().insert("x-yonder-frontend", value);
    }
    response
}

/// Map a request path to a safe relative path, or reject. Every
/// component must be a normal file name — no separators, `.`, `..`,
/// or dot-prefixed entries (blocks `.git`, `.env` and `.staging-*`
/// staging directories inside the registry).
fn safe_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part.starts_with('.') || part.contains(['\\', '%', '\0']) {
                    return None;
                }
                out.push(part);
            }
            Component::RootDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Serve an existing file under the frontend root. Canonicalization
/// happens per resolved file so a path escaping the root (a symlink
/// in the tree, say) is refused rather than followed.
fn serve_file(frontend: &Frontend, rel: &Path) -> Option<Response> {
    let target = frontend.root.join(rel);
    let meta = std::fs::metadata(&target).ok()?;
    if !meta.is_file() {
        return None;
    }
    let canonical_root = std::fs::canonicalize(&frontend.root).ok()?;
    let canonical = std::fs::canonicalize(&target).ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    let bytes = std::fs::read(&canonical).ok()?;
    let mut response = Body::from(bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        content_type(&canonical)
            .parse()
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if cacheable(rel) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    } else {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    Some(response)
}

/// The active frontend's entry document for routes that *are* defined
/// but should still answer browsers with the app shell — e.g.
/// `GET /oauth/authorize` serves this when `Accept: text/html` so the
/// SPA's consent page mounts, while API clients get the route's JSON.
pub fn spa_index(state: &AppState) -> Response {
    let frontend = frontends::resolve_active(&state.config(), None);
    tag(&frontend.name, serve_entry(&frontend))
}

/// SPA entry: the frontend's `index.html` for any unmatched path, or
/// the embedded placeholder when the builtin hasn't been built.
fn serve_entry(frontend: &Frontend) -> Response {
    let entry = frontend
        .manifest
        .entry
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..");
    let path = if entry {
        frontend.root.join(&frontend.manifest.entry)
    } else {
        frontend.root.join("index.html")
    };
    let bytes = std::fs::read(&path).ok();
    let body = match bytes {
        Some(bytes) => Body::from(bytes),
        None if frontend.name == frontends::BUILTIN => Body::from(PLACEHOLDER),
        None => return (StatusCode::NOT_FOUND, "frontend entry not found").into_response(),
    };
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Immutable caching applies only to hashed build artifacts under
/// `/assets/`; documents, the service worker and the manifest must
/// always revalidate so frontend switches take effect promptly.
fn cacheable(rel: &Path) -> bool {
    rel.starts_with("assets")
}

/// Extension → content type for the static bundles frontends ship.
/// `crate::media::images::content_type_for_key` covers the media
/// set; the frontend needs a wider map.
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("webmanifest") => "application/manifest+json",
        Some("json" | "map") => "application/json",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("mp3" | "ogg" | "wav" | "flac" | "opus" | "m4a") => "audio/mpeg",
        Some("mp4" | "webm" | "mov") => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths_are_json_404s() {
        for path in [
            "/api",
            "/api/v1/no-such-endpoint",
            "/.well-known/webfinger",
            "/oauth/authorize",
            "/media/abc123",
            "/swagger-ui/assets/x.css",
            "/openapi.json",
        ] {
            assert!(is_api_path(path), "{path}");
        }
        for path in ["/", "/index.html", "/apisauce", "/oauth2", "/mediax/1"] {
            assert!(!is_api_path(path), "{path}");
        }
    }

    #[test]
    fn safe_path_rejects_unsafe() {
        for raw in [
            "/index.html",
            "/assets/app.abc.js",
            "/deep/dir/file.css",
            "/",
        ] {
            assert!(safe_path(raw).is_some(), "{raw}");
        }
        assert!(safe_path("/").unwrap().as_os_str().is_empty());
        for raw in [
            "/../etc/passwd",
            "/a/../../b",
            "/.git/config",
            "/.env",
            "/assets/../x",
            "/%2e%2e/x",
            "/a\\b",
        ] {
            assert!(safe_path(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn content_type_covers_frontend_assets() {
        assert_eq!(content_type(Path::new("x.js")), "text/javascript");
        assert_eq!(
            content_type(Path::new("x.webmanifest")),
            "application/manifest+json"
        );
        assert_eq!(content_type(Path::new("x.woff2")), "font/woff2");
        assert_eq!(
            content_type(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }
}
