//! Epic-23 alternative-frontends integration tests — zip/path/git
//! installs, the default-frontend selection, SPA serving via the
//! router fallback, and the per-user `frontend` setting — against
//! testcontainers Postgres+Redis.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/e2e/db.rs`).

#![allow(clippy::result_large_err)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    config::{Config, RateLimit},
    http::{build_router, AppState},
};

use crate::harness;

struct Env {
    app: Router,
    state: Arc<AppState>,
}

/// Fresh temp root + a `Config` whose frontends dirs point inside it.
fn base_config() -> (Config, PathBuf) {
    let dir = std::env::temp_dir().join(format!("yonder-fe-e2e-{}", uuid::Uuid::new_v4()));
    let mut config = Config::default();
    config.frontends.dir = dir.join("frontends").to_string_lossy().into();
    config.frontends.builtin_dir = dir.join("builtin").to_string_lossy().into();
    (config, dir)
}

async fn test_env(mut config: Config) -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    for bucket in [
        &mut config.rate_limit.authentication,
        &mut config.rate_limit.account_creation,
        &mut config.rate_limit.relation,
        &mut config.rate_limit.status,
        &mut config.rate_limit.media,
        &mut config.rate_limit.timeline,
        &mut config.rate_limit.search,
        &mut config.rate_limit.feed,
        &mut config.rate_limit.federation_inbox,
    ] {
        *bucket = RateLimit {
            limit: 10_000,
            period_seconds: 60,
            authenticated_limit: None,
        };
    }
    config.federation.base_url = Some("http://127.0.0.1:4000".to_string());
    config.auth.require_email_verification = false;

    let state = Arc::new(AppState::new(config, pool, redis_handle));
    Some(Env {
        app: build_router(state.clone()),
        state,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn register(app: &Router, username: &str, email: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/accounts")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "username": username, "email": email,
                        "password": "hunter2-password", "agreement": true, "locale": "en",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

async fn make_admin(env: &Env, username: &str) {
    sqlx::query("UPDATE accounts SET role = 'admin' WHERE username = $1")
        .bind(username)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE oauth_access_tokens SET scopes = scopes || ' admin' \
         WHERE user_id = (SELECT u.id FROM users u \
                          JOIN accounts a ON a.id = u.account_id \
                          WHERE a.username = $1)",
    )
    .bind(username)
    .execute(&env.state.db)
    .await
    .unwrap();
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or(json!({})).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

/// Like [`request`], but sends `raw` verbatim as the JSON body.
async fn request_raw(
    app: &Router,
    path: &str,
    token: &str,
    raw: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(raw.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

/// Zip bytes for a minimal valid frontend.
fn frontend_zip(index: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file::<_, ()>("index.html", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(index.as_bytes()).unwrap();
    writer
        .start_file::<_, ()>("assets/app.js", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"console.log(1)").unwrap();
    writer.finish().unwrap().into_inner()
}

/// Malicious or invalid zip for a given single entry.
fn one_entry_zip(path: &str) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file::<_, ()>(path, zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"x").unwrap();
    writer.finish().unwrap().into_inner()
}

/// `POST /api/v1/yonder/admin/frontends` as multipart zip.
async fn install_zip(env: &Env, token: &str, name: &str, zip: &[u8]) -> (StatusCode, Vec<u8>) {
    let boundary = "----yonder-test-boundary";
    let mut body = Vec::new();
    body.extend(
        format!("--{boundary}\r\ncontent-disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n")
            .as_bytes(),
    );
    body.extend(
        format!(
            "--{boundary}\r\ncontent-disposition: form-data; name=\"file\"; filename=\"f.zip\"\r\ncontent-type: application/zip\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend(zip);
    body.extend(format!("\r\n--{boundary}--\r\n").as_bytes());

    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/yonder/admin/frontends")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

/// Build a source directory the path-install source can consume.
fn make_source(dir: &Path, name: &str, html: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("index.html"), html).unwrap();
    path
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zip_install_serve_and_remove() {
    let _permit = harness::acquire().await;
    let (config, _dir) = base_config();
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    let (status, body) = install_zip(&env, &admin, "alt", &frontend_zip("<b>ALT</b>")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // Not the default yet — `/` still serves the builtin placeholder.
    let (status, _, body) = request(&env.app, "GET", "/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Yonder"), "{html}");

    // Select it as default, then `/` serves its index.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        Some(json!({"value": "alt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) = request(&env.app, "GET", "/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-yonder-frontend").unwrap().to_str().unwrap(),
        "alt"
    );
    assert!(String::from_utf8_lossy(&body).contains("ALT"));

    // Assets + SPA fallback paths.
    let (status, headers, body) = request(&env.app, "GET", "/assets/app.js", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("javascript"));
    assert!(String::from_utf8_lossy(&body).contains("console.log"));
    let (status, _, body) = request(&env.app, "GET", "/some/spa/route", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("ALT"));

    // Removing the default refuses; back to yonder first, then remove.
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        "/api/v1/yonder/admin/frontends/alt",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        Some(json!({"value": "yonder"})),
    )
    .await;
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        "/api/v1/yonder/admin/frontends/alt",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = request(&env.app, "GET", "/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("Yonder"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malicious_zips_are_rejected() {
    let _permit = harness::acquire().await;
    let (config, _dir) = base_config();
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    for zip in [
        one_entry_zip("../evil.txt"),
        one_entry_zip("index.html/../x"),
        one_entry_zip("readme.txt"), // no index.html
    ] {
        let (status, body) = install_zip(&env, &admin, "evil", &zip).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_install_is_confined_to_root() {
    let _permit = harness::acquire().await;
    let (mut config, dir) = base_config();
    let source_root = dir.join("sources");
    make_source(&source_root, "alt", "<i>PATH</i>");
    config.frontends.install_from_path_root = Some(source_root.to_string_lossy().into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    let outside = make_source(&dir, "outside", "nope");

    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/frontends",
        Some(&admin),
        Some(json!({
            "name": "alt", "source": "path",
            "path": source_root.join("alt").to_str().unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // Outside the root → 422.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/frontends",
        Some(&admin),
        Some(json!({
            "name": "esc", "source": "path",
            "path": outside.to_str().unwrap(),
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_install_gate_and_scheme() {
    let _permit = harness::acquire().await;
    let (mut config, _dir) = base_config();
    config.frontends.git_enabled = true;
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    for url in ["file:///etc/passwd", "git://x.test/r", "/tmp/local"] {
        let (status, _, body) = request(
            &env.app,
            "POST",
            "/api/v1/yonder/admin/frontends",
            Some(&admin),
            Some(json!({"name": "g", "source": "git", "url": url})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{url}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_frontend_setting_roundtrip() {
    let _permit = harness::acquire().await;
    let (config, _dir) = base_config();
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;
    let user = register(&env.app, "user", "u@x.test").await;
    install_zip(&env, &admin, "alt", &frontend_zip("x")).await;

    // Unknown names → 422.
    let (status, _, _) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&user),
        Some(json!({"frontend": "not-installed"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Set → shows in the settings object.
    let (status, _, body) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&user),
        Some(json!({"frontend": "alt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["frontend"], "alt");

    // Reset → null again.
    let (status, _, body) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&user),
        Some(json!({"frontend": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json(&body)["frontend"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_are_not_shadowed() {
    let _permit = harness::acquire().await;
    let (config, _dir) = base_config();
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;
    install_zip(&env, &admin, "alt", &frontend_zip("x")).await;
    request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        Some(json!({"value": "alt"})),
    )
    .await;

    // Real routes still win after the fallback is active.
    let (status, _, _) = request(&env.app, "GET", "/api/v1/instance", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(&env.app, "GET", "/openapi.json", None, None).await;
    assert_eq!(status, StatusCode::OK);
    // Feeds are Mastodon-shaped paths outside `/api` — a defined
    // route that must still win over the SPA fallback.
    let (status, _, _) = request(&env.app, "GET", "/users/admin.atom", None, None).await;
    assert_ne!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(&env.app, "GET", "/swagger-ui/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = request(&env.app, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);

    // Protocol paths must never fall through to the SPA: an unknown
    // API endpoint answers with a JSON 404, not `index.html` —
    // clients parse the body and would choke on HTML.
    for path in [
        "/api/v1/no-such-endpoint",
        "/.well-known/no-such-document",
        "/media/no-such-file",
        "/oauth/no-such-flow",
    ] {
        let (status, _, body) = request(&env.app, "GET", path, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            json(&body)["error"].is_string(),
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_rejects_traversal_names() {
    let _permit = harness::acquire().await;
    let (config, dir) = base_config();
    // Sentinel outside frontends.dir: `dir.join("..")` lands here.
    std::fs::create_dir_all(&dir).unwrap();
    let sentinel = dir.join("keep.txt");
    std::fs::write(&sentinel, b"safe").unwrap();
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    // Axum percent-decodes path captures, so `%2E%2E` reaches the
    // handler as `..` — every one of these must be refused, never
    // reach `remove_dir_all`.
    for name in ["%2E%2E", "..", "%2E%2E%2F%2E%2E", ".staging-x", "UPPER"] {
        let (status, _, body) = request(
            &env.app,
            "DELETE",
            &format!("/api/v1/yonder/admin/frontends/{name}"),
            Some(&admin),
            None,
        )
        .await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::BAD_REQUEST,
            "{name}: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert!(sentinel.is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_install_rejects_oversized_and_malformed_bodies() {
    let _permit = harness::acquire().await;
    let (mut config, dir) = base_config();
    // Path installs enabled, so a request that gets past the body cap
    // fails on the missing path instead of the "disabled" branch —
    // the error message then tells the two outcomes apart.
    config.frontends.install_from_path_root = Some(dir.to_string_lossy().into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let admin = register(&env.app, "admin", "a@x.test").await;
    make_admin(&env, "admin").await;

    // Valid JSON over the 64 KiB install-body cap → 422 from the
    // bounded read, never a hang or an unbounded parse.
    let big = "x".repeat(80 * 1024);
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/frontends",
        Some(&admin),
        Some(json!({"name": "big", "source": "path", "path": big})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("body read failed"),
        "{}",
        String::from_utf8_lossy(&body)
    );

    // Malformed JSON → 422 from the parser.
    let (status, _, body) = request_raw(
        &env.app,
        "/api/v1/yonder/admin/frontends",
        &admin,
        "{not json",
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("invalid JSON body"),
        "{}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_routes_require_admin_scope() {
    let _permit = harness::acquire().await;
    let (config, _dir) = base_config();
    let Some(env) = test_env(config).await else {
        return;
    };
    let user = register(&env.app, "user", "u@x.test").await;

    for (method, path) in [
        ("GET", "/api/v1/yonder/admin/frontends"),
        ("POST", "/api/v1/yonder/admin/frontends"),
        ("DELETE", "/api/v1/yonder/admin/frontends/alt"),
    ] {
        let (status, _, _) = request(&env.app, method, path, Some(&user), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // The user-facing list works for any authenticated user, 401 anon.
    let (status, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/frontends",
        Some(&user),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json(&body)["frontends"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "yonder"));
    let (status, _, _) = request(&env.app, "GET", "/api/v1/yonder/frontends", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
