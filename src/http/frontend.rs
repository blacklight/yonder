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

/// Cookie mirroring `users.frontend` into page loads. Navigations
/// carry no bearer token, so the settings API writes the choice here
/// and [`fallback`]/[`spa_entry`] read it back — this is the only
/// transport that lets a per-user frontend reach serving.
pub const FRONTEND_COOKIE: &str = "yonder_frontend";

/// The `yonder_frontend` cookie value, if present. Values are not
/// URL-decoded: frontend names match `[a-z0-9_-]+`, so anything else
/// simply falls through to the instance default in
/// [`frontends::resolve_active`].
pub(crate) fn frontend_choice(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == FRONTEND_COOKIE).then(|| value.to_string())
    })
}

/// `Set-Cookie` value asserting [`FRONTEND_COOKIE`] to `frontend` —
/// `None` deletes it (follow the instance default). Emitted by the
/// settings endpoints on every read/write so the cookie self-heals:
/// logging in on a new browser re-syncs it on the first settings
/// fetch, and resetting the preference clears a stale value.
/// `Secure` follows the federation base URL scheme.
pub fn frontend_set_cookie(config: &crate::config::Config, frontend: Option<&str>) -> String {
    // Only well-formed names are asserted; anything else (a stale or
    // hand-edited row) is treated as "no choice" rather than echoed
    // back into a cookie.
    let frontend = frontend.filter(|name| frontends::validate_name(name).is_ok());
    let mut cookie = format!(
        "{FRONTEND_COOKIE}={}; Path=/; SameSite=Lax; HttpOnly",
        frontend.unwrap_or("")
    );
    if frontend.is_none() {
        cookie.push_str("; Max-Age=0");
    }
    if crate::activitypub::federation_base_url(config).starts_with("https://") {
        cookie.push_str("; Secure");
    }
    cookie
}

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
    let frontend = frontends::resolve_active(&config, frontend_choice(&headers).as_deref());
    let Some(rel) = safe_path(uri.path()) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    // `/` and every unmatched path fall through to the SPA entry —
    // feed-backed pages get their `<link rel="alternate">` discovery
    // tags injected into `<head>` so scrapers and feed readers can
    // find the RSS/Atom endpoints on the raw HTML too; profile pages
    // also carry `<link rel="me">` for each link-valued field.
    let mut head_extra = feed_discovery(&config.instance.domain, uri.path());
    if let Some(links) = rel_me_links(&state, uri.path()).await {
        head_extra = Some(match head_extra {
            Some(mut extra) => {
                extra.push('\n');
                extra.push_str(&links);
                extra
            }
            None => links,
        });
    }
    let response = if rel.as_os_str().is_empty() {
        serve_entry(&frontend, head_extra.as_deref())
    } else {
        serve_file(&frontend, &rel).unwrap_or_else(|| serve_entry(&frontend, head_extra.as_deref()))
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
/// Deliberately ignores the per-user cookie: the consent flow is part
/// of the builtin SPA, so it always resolves the admin default rather
/// than a user's alternative frontend.
pub fn spa_index(state: &AppState) -> Response {
    let frontend = frontends::resolve_active(&state.config(), None);
    tag(&frontend.name, serve_entry(&frontend, None))
}

/// SPA entry for a request path — resolves the active frontend and
/// injects feed-discovery `<link>` tags when the path is feed-backed.
/// Shared with the feed handlers, whose `/tags/{t}`-style catch-alls
/// claim paths that are also web pages. `user_choice` carries the
/// `yonder_frontend` cookie value so feed-backed pages render in the
/// visitor's chosen frontend too.
pub fn spa_entry(state: &AppState, path: &str, user_choice: Option<&str>) -> Response {
    let config = state.config();
    let frontend = frontends::resolve_active(&config, user_choice);
    let head_extra = feed_discovery(&config.instance.domain, path);
    tag(
        &frontend.name,
        serve_entry(&frontend, head_extra.as_deref()),
    )
}

/// `<link rel="alternate">` feed discovery markup for SPA routes that
/// back a feed: profile pages (`/@user`, also `/@user@this-domain`)
/// map to `/users/{u}` and hashtag pages (`/tags/{t}`) to
/// `/tags/{t}`. Remote accounts, list pages (owner-token feeds are
/// never advertised) and every other path get nothing. `path` is the
/// raw request path — still percent-encoded, which is exactly what the
/// feed `href`s want.
fn feed_discovery(local_domain: &str, path: &str) -> Option<String> {
    if let Some(username) = local_profile_username(local_domain, path) {
        let encoded: String = url::form_urlencoded::byte_serialize(username.as_bytes()).collect();
        return Some(crate::feeds::discovery_links(
            &format!("/users/{}", encoded.replace('+', "%20")),
            &format!("@{username}"),
        ));
    }
    if let Some(rest) = path.strip_prefix("/tags/") {
        if rest.is_empty() || rest.contains('/') {
            return None;
        }
        let decoded = percent_encoding::percent_decode_str(rest)
            .decode_utf8()
            .unwrap_or_else(|_| rest.into());
        return Some(crate::feeds::discovery_links(
            &format!("/tags/{rest}"),
            &format!("#{decoded}"),
        ));
    }
    None
}

/// The decoded local username for a `/@{user}` profile path (also
/// `/@{user}@this-domain`), or `None` for remote handles, sub-paths
/// and non-profile routes. The raw path is percent-encoded; decode
/// for the acct parse.
fn local_profile_username(local_domain: &str, path: &str) -> Option<String> {
    let rest = path.strip_prefix("/@")?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .ok()?;
    // `user@domain` handles are only local when the domain is this
    // instance — remote profiles are served by their origin.
    let username = match decoded.split_once('@') {
        Some((user, domain)) if domain.eq_ignore_ascii_case(local_domain) => user,
        Some(_) => return None,
        None => decoded.as_ref(),
    };
    (!username.is_empty()).then(|| username.to_string())
}

/// `<link rel="me">` tags for a local profile page — one per
/// link-valued profile field so crawlers can verify the claims from
/// the raw HTML (the SPA renders fields client-side). URL selection
/// matches the `rel="me"` verifier itself
/// ([`crate::users::profile::field_verify_url`]): the raw value when
/// it is a linkable URL, otherwise the first anchor's href. Remote
/// handles, unknown/suspended accounts and DB errors inject nothing —
/// the page is still served.
async fn rel_me_links(state: &AppState, path: &str) -> Option<String> {
    let config = state.config();
    let username = local_profile_username(&config.instance.domain, path)?;
    let account = match crate::users::account::load_local_by_username(&state.db, &username).await {
        Ok(account) => account?,
        Err(e) => {
            tracing::warn!(error = %e, "rel=\"me\" profile lookup failed; serving without links");
            return None;
        }
    };
    let mut seen = Vec::new();
    let mut links = String::new();
    for entry in account.profile_fields.as_array()? {
        let Some(url) = crate::users::profile::field_verify_url(&config, entry) else {
            continue;
        };
        if seen.contains(&url) {
            continue;
        }
        if !links.is_empty() {
            links.push('\n');
        }
        links.push_str(&format!(
            "<link rel=\"me\" href=\"{}\" />",
            quick_xml::escape::escape(crate::feeds::render::xml_safe(&url))
        ));
        seen.push(url);
    }
    (!links.is_empty()).then_some(links)
}

/// Splice `extra` markup just before `</head>`. `None` for non-UTF-8
/// documents or markup without a head — the entry is then served
/// untouched rather than guessed at.
fn inject_head(bytes: &[u8], extra: &str) -> Option<Vec<u8>> {
    let html = std::str::from_utf8(bytes).ok()?;
    let idx = html.to_ascii_lowercase().find("</head")?;
    let mut out = String::with_capacity(html.len() + extra.len() + 1);
    out.push_str(&html[..idx]);
    out.push_str(extra);
    out.push('\n');
    out.push_str(&html[idx..]);
    Some(out.into_bytes())
}

/// SPA entry: the frontend's `index.html` for any unmatched path, or
/// the embedded placeholder when the builtin hasn't been built.
/// `head_extra` is extra `<head>` markup (feed discovery links)
/// spliced in for feed-backed routes.
fn serve_entry(frontend: &Frontend, head_extra: Option<&str>) -> Response {
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
        Some(bytes) => Body::from(
            head_extra
                .and_then(|extra| inject_head(&bytes, extra))
                .unwrap_or(bytes),
        ),
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
    fn feed_discovery_maps_profile_and_tag_paths() {
        let profile = feed_discovery("example.com", "/@alice").unwrap();
        assert!(profile.contains("href=\"/users/alice.atom\""));
        assert!(profile.contains("href=\"/users/alice.rss\""));
        // A fully-qualified local handle unwraps to the same feed.
        let fqn = feed_discovery("example.com", "/@alice@example.com").unwrap();
        assert!(fqn.contains("href=\"/users/alice.atom\""));

        let tag = feed_discovery("example.com", "/tags/rust").unwrap();
        assert!(tag.contains("href=\"/tags/rust.atom\""));
        // Percent-encoded tag: the href keeps the raw segment, the
        // title shows the decoded tag.
        let uni = feed_discovery("example.com", "/tags/caf%C3%A9").unwrap();
        assert!(uni.contains("href=\"/tags/caf%C3%A9.atom\""));
        assert!(uni.contains("title=\"#café\""));
    }

    #[test]
    fn feed_discovery_skips_unserved_paths() {
        for path in [
            "/",                  // no feed for the landing page
            "/@alice@remote.tld", // remote profile — origin serves it
            "/@alice/extra",      // sub-paths carry no feed
            "/@/followers",
            "/tags/rust/related",
            "/tags/",
            "/lists/1", // owner-token feed — never advertised
            "/about",
        ] {
            assert!(feed_discovery("example.com", path).is_none(), "{path}");
        }
    }

    #[test]
    fn inject_head_splices_before_closing_tag() {
        let doc = b"<html><head><title>x</title></HEAD><body/></html>";
        let out = inject_head(doc, "<link />").unwrap();
        let html = String::from_utf8(out).unwrap();
        assert!(html.contains("<title>x</title><link />\n</HEAD>"));
        // No head → untouched (None), never guessed.
        assert!(inject_head(b"<html><body/></html>", "x").is_none());
        assert!(inject_head(&[0xff, 0xfe], "x").is_none());
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
