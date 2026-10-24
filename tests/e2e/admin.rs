//! Epic-22 admin-panel integration tests — config overlays (describe,
//! set, reset, live reload, secret redaction, read-only sections),
//! the audit-log reader, relays, admin user actions, the real
//! `EmailSend` handler against a recording transport, and VAPID key
//! generation — against testcontainers Postgres+Redis.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;
use yonder::{
    admin::email::{EmailSendJob, EmailTransport},
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    moderation::audit,
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";

struct Env {
    app: Router,
    state: Arc<AppState>,
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
    config.federation.base_url = Some(BASE.to_string());
    config.auth.require_email_verification = false;
    // Pretend mail is configured so the job doesn't early-skip — the
    // recording transport stands in for SMTP.
    config.email.smtp_host = Some("smtp.test".into());

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

/// Promote `username` to a role and grant the token the `admin` scope.
async fn make_role(env: &Env, username: &str, role: &str) {
    sqlx::query("UPDATE accounts SET role = $2 WHERE username = $1")
        .bind(username)
        .bind(role)
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

async fn account_id(env: &Env, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(username)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
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
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, headers, body)
}

fn job_ctx(env: &Env) -> JobContext {
    JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    }
}

/// One captured message: `(from, to, subject, text, html)`.
type SentMail = (String, String, String, String, Option<String>);

/// Recording transport — captures every `send`.
struct RecordingTransport {
    sent: Mutex<Vec<SentMail>>,
}

impl RecordingTransport {
    async fn sent(&self) -> Vec<SentMail> {
        self.sent.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl EmailTransport for RecordingTransport {
    async fn send(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        text: &str,
        html: Option<&str>,
    ) -> yonder::Result<()> {
        self.sent.lock().await.push((
            from.to_string(),
            to.to_string(),
            subject.to_string(),
            text.to_string(),
            html.map(|h| h.to_string()),
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Overlay settings
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_describe_set_reset() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "mod", "moderator").await;

    // Every registry key is described with introspection metadata.
    let (status, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body.as_array().unwrap();
    assert!(entries.len() > 50);
    let name = entries
        .iter()
        .find(|e| e["toml_path"] == "instance.name")
        .unwrap();
    assert_eq!(name["env"], "YONDER_INSTANCE__NAME");
    assert_eq!(name["source"], "default");
    assert_eq!(name["writable"], true);
    assert_eq!(name["requires_restart"], false);

    // Section filter.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings?section=rate_limit",
        Some(&admin),
        None,
    )
    .await;
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["toml_path"].as_str().unwrap().starts_with("rate_limit.")));

    // Set an overlay → effective value + source change live.
    let (status, _, body) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/instance.name",
        Some(&admin),
        Some(json!({"value": "Testville"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["value"], "Testville");
    assert_eq!(body["source"], "overlay");
    // Live reload: a handler read sees the new value immediately.
    assert_eq!(env.state.config().instance.name, "Testville");

    // Reset → falls back to the default.
    let (status, _, body) = request(
        &env.app,
        "DELETE",
        "/api/v1/yonder/admin/settings/instance.name",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "default");
    assert_eq!(
        env.state.config().instance.name,
        Config::default().instance.name
    );
    // Reset is idempotent.
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        "/api/v1/yonder/admin/settings/instance.name",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Read-only section → 403; unknown key → 404; bad type → 422.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/database.url",
        Some(&admin),
        Some(json!({"value": "postgres://x"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/instance.nope",
        Some(&admin),
        Some(json!({"value": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/server.port",
        Some(&admin),
        Some(json!({"value": "not-a-port"})),
    )
    .await;
    // Read-only wins over type validation.
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/polls.max_options",
        Some(&admin),
        Some(json!({"value": "not-a-number"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Every infrastructure section rejects writes.
    for key in [
        "server.port",
        "database.max_connections",
        "redis.pool_size",
        "jobs.concurrency",
    ] {
        let (status, _, body) = request(
            &env.app,
            "PUT",
            &format!("/api/v1/yonder/admin/settings/{key}"),
            Some(&admin),
            Some(json!({"value": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{key}: {body}");
    }

    // `requires_restart` marks startup-derived keys: infra sections,
    // rate-limit buckets, media backend, cron schedules.
    for (key, expected) in [
        ("database.max_connections", true),
        ("rate_limit.search.limit", true),
        ("media.backend", true),
        ("media.orphan_cleanup_schedule", true),
        ("instance.name", false),
    ] {
        let (status, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/yonder/admin/settings/{key}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{key}: {body}");
        assert_eq!(body["requires_restart"], expected, "{key}: {body}");
    }

    // Two overlay writes → two audit rows.
    let overlay_audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action LIKE 'config.%'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(overlay_audits, 3, "set + reset + reset again");

    // Moderators can't touch the settings surface.
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings",
        Some(&mod_),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_redact_secrets() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;

    let (status, _, body) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/settings/email.smtp_password",
        Some(&admin),
        Some(json!({"value": "s3cret"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["value"], Value::Null);
    assert_eq!(body["set"], true);
    assert_eq!(body["sensitive"], true);

    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings/email.smtp_password",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body["value"], Value::Null);
    assert_eq!(body["set"], true);

    // Connection strings carry passwords: neither the live value nor the
    // default leaks, even though the sections are read-only.
    for key in ["database.url", "redis.url"] {
        let (status, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/yonder/admin/settings/{key}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["sensitive"], true);
        assert_eq!(body["value"], Value::Null);
        assert_eq!(body["default"], Value::Null);
        assert!(!body.to_string().contains("postgres://"), "{body}");
        assert!(!body.to_string().contains("redis://"), "{body}");
    }
    let (_, _, all) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings?section=database",
        Some(&admin),
        None,
    )
    .await;
    assert!(!all.to_string().contains("yonder:yonder"), "{all}");

    // The audit row redacts the secret value.
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_log WHERE action = 'config.set' AND details->>'key' = 'email.smtp_password'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(details["value"], "<redacted>");
}

// ---------------------------------------------------------------------------
// Audit log reader
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_log_filters_and_pagination() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let admin_id = account_id(&env, "admin").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "mod", "moderator").await;

    for i in 0..5 {
        audit::record(
            &env.state.db,
            Some(admin_id),
            "report.resolve",
            "report",
            Some(i),
            json!({"i": i}),
        )
        .await
        .unwrap();
    }
    audit::record(
        &env.state.db,
        Some(admin_id),
        "config.set",
        "config",
        None,
        json!({}),
    )
    .await
    .unwrap();

    // All entries, newest first.
    let (status, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 6);
    assert_eq!(body[0]["action"], "config.set");
    assert_eq!(body[0]["actor_account"]["username"], "admin");

    // Prefix filter.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?action=report.",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 5);

    // Target filter.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?target_type=config",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Keyset pagination: take the first page of two, then follow
    // max_id.
    let (_, headers, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?limit=2",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert!(headers.contains_key("link"));
    let max_id = body[1]["id"].as_str().unwrap();
    let (_, _, page2) = request(
        &env.app,
        "GET",
        &format!("/api/v1/admin/audit_log?limit=2&max_id={max_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(page2.as_array().unwrap().len(), 2);
    assert_ne!(page2[0]["id"], body[0]["id"]);

    // Admin-only — a moderator is denied.
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log",
        Some(&mod_),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_log_rejects_bad_filters_and_escapes_prefix() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let admin_id = account_id(&env, "admin").await;
    audit::record(
        &env.state.db,
        Some(admin_id),
        "report.resolve",
        "report",
        Some(1),
        json!({}),
    )
    .await
    .unwrap();

    // Malformed ids are 422, not silently dropped (which would widen the set).
    for query in ["actor_account_id=abc", "target_id=x1", "since=yesterday"] {
        let (status, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/admin/audit_log?{query}"),
            Some(&admin),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}: {body}");
    }

    // LIKE metacharacters match literally: `_` is not a wildcard.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?action=_",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 0, "{body}");
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?action=report_",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 0, "{body}");
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/admin/audit_log?action=report.",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
}

// ---------------------------------------------------------------------------
// Relays
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_lifecycle() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "mod", "moderator").await;

    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/admin/relays",
        Some(&admin),
        Some(json!({"inbox_url": "https://relay.example/inbox"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "pending");
    let relay_id = body["id"].as_str().unwrap().to_string();

    // Duplicate → 409.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/admin/relays",
        Some(&admin),
        Some(json!({"inbox_url": "https://relay.example/inbox"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Bad URL → 422.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/admin/relays",
        Some(&admin),
        Some(json!({"inbox_url": "not-a-url"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Authorize → accepted.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/relays/{relay_id}/authorize"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "accepted");
    // Re-authorize → 422.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/relays/{relay_id}/authorize"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // List shows the relay.
    let (_, _, body) = request(&env.app, "GET", "/api/v1/admin/relays", Some(&admin), None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Moderators can't manage relays.
    let (status, _, _) = request(&env.app, "GET", "/api/v1/admin/relays", Some(&mod_), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Delete.
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/admin/relays/{relay_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM relays")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);

    // Lifecycle wrote three audit rows.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action LIKE 'relay.%'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(n, 3);
}

// ---------------------------------------------------------------------------
// Admin user actions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_hygiene_actions() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let _user = register(&env.app, "user", "user@x.test").await;
    let user_id = account_id(&env, "user").await;

    for (ty, check) in [
        (
            "verify_email",
            "SELECT email_verified_at IS NOT NULL FROM users WHERE account_id = $1",
        ),
        (
            "require_password_reset",
            "SELECT require_password_reset FROM users WHERE account_id = $1",
        ),
    ] {
        let (status, _, body) = request(
            &env.app,
            "POST",
            &format!("/api/v1/admin/accounts/{user_id}/action"),
            Some(&admin),
            Some(json!({"type": ty})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{ty}: {body}");
        let flag: bool = sqlx::query_scalar(check)
            .bind(user_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
        assert!(flag, "{ty} did not take effect");
    }

    // disable_2fa on an account without 2FA is a no-op but succeeds.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{user_id}/action"),
        Some(&admin),
        Some(json!({"type": "disable_2fa"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Each action wrote one moderation_actions + audit row.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moderation_actions WHERE account_id = $1")
            .bind(user_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(n, 3);

    // reset_password returns the link.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{user_id}/reset_password"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = body["reset_url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{BASE}/auth/password-reset/")));

    // email_reset confirms the send.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{user_id}/email_reset"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], true);

    // Self-action rejected.
    let admin_id = account_id(&env, "admin").await;
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{admin_id}/reset_password"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_links_respect_staff_guard() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let admin_id = account_id(&env, "admin").await;
    let moderator = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "mod", "moderator").await;
    let _other_mod = register(&env.app, "mod2", "mod2@x.test").await;
    make_role(&env, "mod2", "moderator").await;
    let mod2_id = account_id(&env, "mod2").await;
    let _user = register(&env.app, "user", "user@x.test").await;
    let user_id = account_id(&env, "user").await;

    // A moderator cannot mint reset links for staff.
    for verb in ["reset_password", "email_reset"] {
        for staff in [admin_id, mod2_id] {
            let (status, _, body) = request(
                &env.app,
                "POST",
                &format!("/api/v1/admin/accounts/{staff}/{verb}"),
                Some(&moderator),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{verb} on {staff}: {body}");
        }
        let (status, _, body) = request(
            &env.app,
            "POST",
            &format!("/api/v1/admin/accounts/{user_id}/{verb}"),
            Some(&moderator),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{verb} on user: {body}");
    }

    // An admin may target staff.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{mod2_id}/reset_password"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Suspended accounts are rejected.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{user_id}/action"),
        Some(&admin),
        Some(json!({"type": "suspend"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{user_id}/reset_password"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ---------------------------------------------------------------------------
// Email templates + transport
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn email_templates_and_send() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let transport = Arc::new(RecordingTransport {
        sent: Mutex::new(Vec::new()),
    });
    yonder::admin::email::set_test_transport(transport.clone());

    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let _user = register(&env.app, "user", "user@x.test").await;
    let user_id = account_id(&env, "user").await;

    // Seeded defaults exist and are readable.
    let (status, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/email_templates/welcome",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["subject"]
        .as_str()
        .unwrap()
        .contains("{{instance_name}}"));

    // Unknown kind → 404.
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/email_templates/nope",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Edit the welcome template.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/email_templates/welcome",
        Some(&admin),
        Some(json!({
            "subject": "Hi {{user_handle}} at {{instance_name}}",
            "text_body": "Welcome aboard, {{user_handle}}!",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Template-shaped payload renders + sends through the transport.
    let ctx = job_ctx(&env);
    EmailSendJob
        .run(&ctx, &json!({"kind": "welcome", "account_id": user_id}))
        .await
        .unwrap();
    let sent = transport.sent().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "user@x.test");
    assert_eq!(sent[0].2, "Hi user at Yonder");
    assert_eq!(sent[0].3, "Welcome aboard, user!");

    // Pre-rendered payloads go through as-is (never templated).
    EmailSendJob
        .run(
            &ctx,
            &json!({
                "kind": "verify_email",
                "to": "user@x.test",
                "subject": "Verify",
                "text_body": "token: abc",
            }),
        )
        .await
        .unwrap();
    let sent = transport.sent().await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].3, "token: abc");

    // Missing template → skipped, no error.
    EmailSendJob
        .run(&ctx, &json!({"kind": "mention", "account_id": user_id}))
        .await
        .unwrap();
    assert_eq!(transport.sent().await.len(), 2);

    // Skip paths ack silently: missing recipient key, unknown account,
    // class flag off, no smtp_host configured.
    EmailSendJob
        .run(&ctx, &json!({"kind": "welcome"}))
        .await
        .unwrap();
    EmailSendJob
        .run(&ctx, &json!({"kind": "welcome", "account_id": 999_999}))
        .await
        .unwrap();
    let mut off = job_ctx(&env);
    Arc::make_mut(&mut off.config).email.send_welcome = false;
    EmailSendJob
        .run(&off, &json!({"kind": "welcome", "account_id": user_id}))
        .await
        .unwrap();
    let mut no_smtp = job_ctx(&env);
    Arc::make_mut(&mut no_smtp.config).email.smtp_host = None;
    EmailSendJob
        .run(&no_smtp, &json!({"kind": "welcome", "account_id": user_id}))
        .await
        .unwrap();
    assert_eq!(transport.sent().await.len(), 2);

    // `force` overrides a disabled class flag.
    EmailSendJob
        .run(
            &off,
            &json!({"kind": "welcome", "account_id": user_id, "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(transport.sent().await.len(), 3);

    // HTML bodies escape substituted values; text subjects don't.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/email_templates/welcome",
        Some(&admin),
        Some(json!({
            "subject": "Hi {{user_handle}}",
            "text_body": "Welcome, {{user_handle}}!",
            "html_body": "<p>{{note}}</p>",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    EmailSendJob
        .run(
            &ctx,
            &json!({
                "kind": "welcome",
                "account_id": user_id,
                "note": "<i>&\"x\"</i>",
            }),
        )
        .await
        .unwrap();
    let sent = transport.sent().await;
    assert_eq!(sent.len(), 4);
    assert_eq!(
        sent[3].4.as_deref(),
        Some("<p>&lt;i&gt;&amp;&quot;x&quot;&lt;/i&gt;</p>")
    );

    // Test-send endpoint hits the same transport.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/email/test",
        Some(&admin),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sent"], true);
    assert_eq!(transport.sent().await.len(), 5);
}

// ---------------------------------------------------------------------------
// VAPID keys
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webpush_generate_keys() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "mod", "moderator").await;

    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/webpush/generate_keys",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let public = body["public_key"].as_str().unwrap();
    // Uncompressed P-256 point: 65 bytes, base64url-unpadded.
    let raw = URL_SAFE_NO_PAD.decode(public).unwrap();
    assert_eq!(raw.len(), 65);
    assert_eq!(raw[0], 0x04);
    // Never the private key.
    assert!(body.get("private_key").is_none());

    // Stored as overlays; settings API reports set-but-redacted.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/settings/http.vapid_private_key",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body["set"], true);
    assert_eq!(body["value"], Value::Null);
    assert_eq!(
        env.state.config().http.vapid_public_key.as_deref(),
        Some(public)
    );

    // Audited with a redacted private key.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'webpush.generate_keys'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(n, 1);

    // Moderator → 403.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/admin/webpush/generate_keys",
        Some(&mod_),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Frontend selection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frontend_selection() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;

    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(body["value"], "yonder");

    let (status, _, body) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        Some(json!({"value": "mastodon-fe"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(env.state.config().admin.default_frontend, "mastodon-fe");

    // Empty → 422.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        "/api/v1/yonder/admin/frontend",
        Some(&admin),
        Some(json!({"value": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
