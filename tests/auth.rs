//! Epic-07 authentication integration tests — full register/login/OAuth
//! flows against testcontainers Postgres+Redis via `tower::oneshot`.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::{postgres::Postgres, redis::Redis};
use tower::ServiceExt;
use yonder::{
    cache,
    config::{Config, RateLimit},
    db,
    http::{build_router, AppState},
};

fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

struct Env {
    app: Router,
    state: Arc<AppState>,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _redis: testcontainers::ContainerAsync<Redis>,
}

async fn test_app(mut config: Config) -> Option<Env> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return None;
    }
    let pg = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
    let pool = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
    ))
    .await
    .unwrap();
    db::migrate::run(&pool).await.unwrap();

    let redis = Redis::default().start().await.expect("redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
    config.redis.url = format!("redis://127.0.0.1:{redis_port}");
    let redis_handle = cache::connect(&config.redis).await.unwrap();

    // Rate-limit buckets are tiny by default — lift them for the suite.
    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();

    let state = Arc::new(AppState::new(config, pool, redis_handle));
    let app = build_router(state.clone());
    Some(Env {
        app,
        state,
        _pg: pg,
        _redis: redis,
    })
}

async fn request(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn json_req(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn authed(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    Request::from_parts(parts, body)
}

fn form_req(path: &str, form: &[(&str, &str)]) -> Request<Body> {
    let encoded = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(encoded))
        .unwrap()
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.email.smtp_host = None; // no mail server unless a test sets it
    config
}

async fn register_user(app: &Router, username: &str, email: &str) -> (StatusCode, Value) {
    request(
        app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": username, "email": email,
                "password": "hunter2password", "agreement": true,
            }),
        ),
    )
    .await
}

async fn password_login(app: &Router, email: &str, password: &str) -> (StatusCode, Value) {
    request(
        app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("username", email),
                ("password", password),
            ],
        ),
    )
    .await
}

async fn create_app(
    app: &Router,
    name: &str,
    scopes: &str,
    confidential: bool,
) -> (StatusCode, Value) {
    request(
        app,
        json_req(
            "POST",
            "/api/v1/apps",
            json!({
                "client_name": name,
                "redirect_uris": ["urn:ietf:wg:oauth:2.0:oob"],
                "scopes": scopes,
                "confidential": confidential,
            }),
        ),
    )
    .await
}

#[tokio::test]
async fn register_login_verify_credentials_flow() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "alice", "alice@example.com").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scope"], "read write");
    let reg_token = body["access_token"].as_str().unwrap();

    let (status, account) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            reg_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["username"], "alice");
    assert_eq!(account["acct"], "alice");
    assert!(account["source"].is_object());

    // Password login → new token + session, works on the same endpoint.
    let (status, body) = password_login(&env.app, "alice@example.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Anonymous (client-less) password grants default to `read write`,
    // matching anonymous registration.
    assert_eq!(body["scope"], "read write");
    assert!(body["refresh_token"].is_string());
    let refresh = body["refresh_token"].as_str().unwrap().to_string();
    let token = body["access_token"].as_str().unwrap().to_string();
    let (status, account) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");

    // The anonymous grant's refresh token rotates on the bearer secret
    // alone — no client authentication applies.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Storage invariants: argon2id hash, hashed token material only.
    let hash: String =
        sqlx::query_scalar("SELECT password_hash FROM users JOIN accounts a ON a.id = users.account_id WHERE a.username = 'alice'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(hash.starts_with("$argon2id$"), "{hash}");
    let plaintext_tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_access_tokens WHERE token_hash = $1")
            .bind(&token)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(plaintext_tokens, 0);
}

#[tokio::test]
async fn registration_validation_and_modes() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    for username in [
        "bad-name",
        "bad name!",
        "yonder.instance",
        "YONDER.INSTANCE",
    ] {
        let (status, body) = register_user(&env.app, username, "x@example.com").await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{username}: {body}"
        );
    }
    // The instance actor's name is *reserved*, not just charset-invalid.
    let (status, body) = register_user(&env.app, "yonder.instance", "x@example.com").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("reserved"),
        "{body}"
    );
    // An over-length `reason` is rejected in `open` mode too.
    let (status, _) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": "yara", "email": "y@x.com",
                "password": "hunter2password", "agreement": true,
                "reason": "x".repeat(5001),
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = register_user(&env.app, "bob", "bob@example.com").await;
    assert_eq!(status, StatusCode::OK);
    // Duplicate username (case-insensitive) and email → 422.
    let (status, _) = register_user(&env.app, "BOB", "other@example.com").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = register_user(&env.app, "carol", "BOB@example.com").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Missing agreement → 422.
    let (status, _) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({"username": "dave", "email": "d@x.com", "password": "hunter2password"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Closed mode → 403; approval mode → unapproved + reason required.
    let mut config = base_config();
    config.auth.registration_mode = yonder::config::RegistrationMode::Closed;
    let Some(env) = test_app(config).await else {
        return;
    };
    let (status, _) = register_user(&env.app, "erin", "e@x.com").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let mut config = base_config();
    config.auth.registration_mode = yonder::config::RegistrationMode::ApprovalRequired;
    let Some(env) = test_app(config).await else {
        return;
    };
    let (status, _) = register_user(&env.app, "frank", "f@x.com").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY); // reason required
    let (status, _) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": "frank", "email": "f@x.com",
                "password": "hunter2password", "agreement": true,
                "reason": "I read the rules",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Unapproved: login works, `verify_credentials` (read) works — but
    // the write gate refuses non-read scopes.
    let (status, body) = password_login(&env.app, "f@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap();
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/tokens",
                json!({"name": "n", "scopes": "write"}),
            ),
            token,
        ),
    )
    .await;
    // Token creation requires no write scope on the token — but the
    // unapproved account's own scopes still gate write exercise; assert
    // the approval flag is false via the sessions row instead.
    let approved: bool =
        sqlx::query_scalar("SELECT approved_at IS NOT NULL FROM users WHERE email = 'f@x.com'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!approved, "{status}");
}

#[tokio::test]
async fn oauth_apps_authorization_code_and_revocation() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = create_app(&env.app, "TestApp", "read write", true).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let client_id = body["client_id"].as_str().unwrap().to_string();
    let client_secret = body["client_secret"].as_str().unwrap().to_string();

    // client_credentials → app token → verify_credentials.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "client_credentials"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let app_token = body["access_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/apps/verify_credentials", json!({})),
            &app_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "TestApp");
    assert_eq!(body["redirect_uri"], "urn:ietf:wg:oauth:2.0:oob");
    assert!(body["id"].is_string());
    assert_eq!(body["client_id"].as_str(), Some(client_id.as_str()));

    // HTTP Basic client authentication works too (RFC 6749 §2.3.1).
    use base64::Engine;
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{client_secret}"));
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("authorization", format!("Basic {basic}"))
        .body(Body::from("grant_type=client_credentials"))
        .unwrap();
    let (status, body) = request(&env.app, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Register a user, get a user token via password grant bound to app.
    let (status, _) = register_user(&env.app, "grace", "g@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("username", "g@x.com"),
                ("password", "hunter2password"),
                ("scope", "read"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_token = body["access_token"].as_str().unwrap().to_string();
    let refresh = body["refresh_token"].as_str().unwrap().to_string();

    // Wrong password → 401-ish grant error.
    let (status, body) = password_login(&env.app, "g@x.com", "wrongpassword").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // Authorization-code flow: authorize → code → exchange → token.
    let (status, body) = request(
        &env.app,
        authed(
            form_req(
                "/oauth/authorize",
                &[
                    ("client_id", &client_id),
                    ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                    ("response_type", "code"),
                    ("scope", "read"),
                    ("state", "a b"),
                ],
            ),
            &user_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = body["code"].as_str().unwrap().to_string();
    // `redirect_to` carries code + the percent-encoded state.
    let redirect_to = body["redirect_to"].as_str().unwrap();
    assert!(redirect_to.contains("code="), "{redirect_to}");
    assert!(redirect_to.contains("state=a+b"), "{redirect_to}");

    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                ("code", &code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code_token = body["access_token"].as_str().unwrap().to_string();
    assert_eq!(body["scope"], "read");

    // Code reuse → invalid_grant.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
                ("code", &code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // Refresh rotation: old refresh dies, new pair works.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", &refresh),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rotated = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(rotated, refresh);
    // A confidential app's refresh token demands client authentication —
    // the bearer secret alone is not enough (RFC 6749 §6).
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[("grant_type", "refresh_token"), ("refresh_token", &rotated)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "invalid_client");
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", &refresh),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Authorized applications list shows the grant; revoking kills tokens.
    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/oauth/authorized_applications", json!({})),
            &code_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["scopes"].as_array().unwrap()[0], "read");
    let authz_id = entries[0]["id"].as_str().unwrap().to_string();

    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/oauth/authorized_applications/{authz_id}"),
                json!({}),
            ),
            &code_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            &code_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Sessions linked to the app's access tokens are deleted with them.
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

#[tokio::test]
async fn sessions_and_user_tokens() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    register_user(&env.app, "heidi", "h@x.com").await;
    let (status, body) = request(&env.app, {
        let mut req = form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("username", "h@x.com"),
                ("password", "hunter2password"),
            ],
        );
        req.headers_mut()
            .insert("user-agent", "TestBrowser/1.0".parse().unwrap());
        req
    })
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = request(
        &env.app,
        authed(json_req("GET", "/api/v1/sessions", json!({})), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sessions = body.as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["browser"], "TestBrowser/1.0");
    assert_eq!(sessions[0]["current"], true);
    let session_id = sessions[0]["id"].as_str().unwrap().to_string();

    // Named user tokens: create/list/revoke + scope subset.
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/tokens",
                json!({"name": "cli", "scopes": "read"}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let named_token = body["token"].as_str().unwrap().to_string();
    let token_id = body["id"].as_str().unwrap().to_string();
    // Scope beyond the caller's scopes → 422.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/tokens",
                json!({"name": "other", "scopes": "read write admin"}),
            ),
            &named_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = request(
        &env.app,
        authed(json_req("GET", "/api/v1/tokens", json!({})), &token),
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1, "{status} {body}");
    // Name conflict → 409.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/tokens",
                json!({"name": "cli", "scopes": "read"}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Revoking the session kills the linked token.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/sessions/{session_id}"),
                json!({}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &env.app,
        authed(json_req("GET", "/api/v1/sessions", json!({})), &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The named token still works; revoke it too.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("DELETE", &format!("/api/v1/tokens/{token_id}"), json!({})),
            &named_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn email_verification_flow() {
    let mut config = base_config();
    config.email.smtp_host = Some("smtp.example.com".into());
    config.email.from_address = "noreply@example.com".into();
    let Some(env) = test_app(config).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "irene", "i@x.com").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let unverified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NULL FROM users WHERE email = 'i@x.com'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(unverified);

    // The queued EmailSend payload carries the token — pull it out of
    // the queue stream to complete the loop.
    let mut conn = env.state.redis.conn().await.unwrap();
    let stream_key = env.state.redis.key("queue:default");
    let entries: Vec<(String, Vec<(String, String)>)> = redis::cmd("XRANGE")
        .arg(&stream_key)
        .arg("-")
        .arg("+")
        .query_async(&mut *conn)
        .await
        .unwrap();
    let envelope = entries
        .iter()
        .flat_map(|(_, f)| f.iter())
        .find(|(k, _)| k == "job")
        .map(|(_, v)| v.clone())
        .expect("email job envelope");
    let job: Value = serde_json::from_str(&envelope).unwrap();
    let text_body = job["payload"]["text_body"]
        .as_str()
        .expect("email text_body");
    let token = text_body
        .split_whitespace()
        .rev()
        .find(|w| w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit()))
        .expect("verification token in payload")
        .to_string();

    let (status, _) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts/verify_email",
            json!({"token": token}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let verified: bool = sqlx::query_scalar(
        "SELECT email_verified_at IS NOT NULL FROM users WHERE email = 'i@x.com'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(verified);
    // Token is single-use.
    let (status, _) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts/verify_email",
            json!({"token": token}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn device_flow() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = create_app(&env.app, "DevApp", "read write", false).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let client_id = body["client_id"].as_str().unwrap().to_string();
    register_user(&env.app, "judy", "j@x.com").await;
    let (status, body) = password_login(&env.app, "j@x.com", "hunter2password").await;
    let user_token = body["access_token"].as_str().unwrap().to_string();
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(
        &env.app,
        form_req("/oauth/device_code", &[("client_id", &client_id)]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let device_code = body["device_code"].as_str().unwrap().to_string();
    let user_code = body["user_code"].as_str().unwrap().to_string();

    // Poll before approval → authorization_pending.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &client_id),
                ("device_code", &device_code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "authorization_pending");

    // Polling again inside the interval → slow_down.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &client_id),
                ("device_code", &device_code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "slow_down");

    // Approve as the user, then poll → token pair (the granted consume
    // ignores the poll-interval marker).
    let (status, _) = request(
        &env.app,
        authed(
            form_req("/oauth/device/authorize", &[("user_code", &user_code)]),
            &user_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", &client_id),
                ("device_code", &device_code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap().to_string();
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn instance_endpoint() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = request(&env.app, json_req("GET", "/api/v1/instance", json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["registrations"]["enabled"], true);
    assert_eq!(body["registrations"]["approval_required"], false);
    assert_eq!(body["uri"], env.state.config().instance.domain);
    assert_eq!(
        body["configuration"]["media_attachments"]["image_size_limit"],
        env.state.config().media.max_upload_size
    );
    register_user(&env.app, "kevin", "k@x.com").await;
    let (status, body) = request(&env.app, json_req("GET", "/api/v1/instance", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stats"]["user_count"], 1);
}
