//! Epic-21 moderation integration tests — report filing + the admin
//! queue, user actions, domain blocks and the audit log — against
//! testcontainers Postgres+Redis.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use pubby_sdk::Activity;
use redis::AsyncCommands;
use serde_json::{json, Value};
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::{postgres::Postgres, redis::Redis};
use tower::ServiceExt;
use yonder::{
    activitypub, cache,
    config::{Config, RateLimit},
    db,
    http::{build_router, AppState},
    jobs::{queue, JobContext, JobEnvelope, JobHandler, JobKind},
    moderation::audit::AuditLogCleanupJob,
    notifications::fanout::NotificationFanoutJob,
};

const BASE: &str = "http://127.0.0.1:4000";

fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

struct Env {
    app: Router,
    state: Arc<AppState>,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _redis: testcontainers::ContainerAsync<Redis>,
}

async fn test_env(mut config: Config) -> Option<Env> {
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
    for bucket in [
        &mut config.rate_limit.authentication,
        &mut config.rate_limit.account_creation,
        &mut config.rate_limit.relation,
        &mut config.rate_limit.status,
        &mut config.rate_limit.media,
        &mut config.rate_limit.timeline,
    ] {
        *bucket = RateLimit {
            limit: 10_000,
            period_seconds: 60,
            authenticated_limit: None,
        };
    }
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-mod-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();

    let dispatcher = yonder::jobs::Dispatcher::new(&redis_handle, &config.jobs);
    let mut state = AppState::new(config, pool, redis_handle);
    let ap = activitypub::build(
        state.config.as_ref(),
        &state.db,
        &state.redis,
        &dispatcher,
        &state.bus,
    )
    .await
    .unwrap();
    state.ap = Some(ap);
    let state = Arc::new(state);
    Some(Env {
        app: build_router(state.clone()),
        state,
        _pg: pg,
        _redis: redis,
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

/// Promote `username` to a role and grant the token the `admin` scope
/// so coverage checks pass.
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
        config: env.state.config.clone(),
        media: env.state.media.clone(),
    }
}

/// Drain the `default` job stream (the test stands in for the worker).
async fn drain_default_queue(env: &Env) -> Vec<JobEnvelope> {
    let key = env.state.redis.key("queue:default");
    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn.xrange(&key, "-", "+").await.unwrap();
    let mut out = Vec::new();
    for entry in reply.ids {
        if let Some(v) = entry.map.get("job") {
            let body: String = redis::from_redis_value(v).unwrap();
            out.push(queue::decode(&body).unwrap());
        }
        let _: i64 = conn.xdel(&key, &[entry.id]).await.unwrap();
    }
    out
}

/// Run every pending `NotificationFanout` job so `admin.report`
/// notifications materialize.
async fn run_pending_fanouts(env: &Env) {
    for envelope in drain_default_queue(env).await {
        if envelope.kind == JobKind::NotificationFanout {
            NotificationFanoutJob
                .run(&job_ctx(env), &envelope.payload)
                .await
                .unwrap();
        }
    }
    let _ = drain_default_queue(env).await;
}

async fn audit_count(env: &Env, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

#[tokio::test]
async fn report_filing_notifies_and_lists() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let _admin = register(&env.app, "admin", "admin@x.test").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "admin", "admin").await;
    make_role(&env, "mod", "moderator").await;
    let alice = register(&env.app, "alice", "alice@x.test").await;
    let _bob = register(&env.app, "bob", "bob@x.test").await;
    let bob_id = account_id(&env, "bob").await;

    // File a report.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/reports",
        Some(&alice),
        Some(json!({
            "account_id": bob_id,
            "comment": "spammy behaviour",
            "category": "spam",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["category"], "spam");
    assert_eq!(body["action_taken"], false);
    run_pending_fanouts(&env).await;

    // The admin sees it open; the report notification went to admins
    // *and* the moderator but not the reporter.
    let (status, _, list) =
        request(&env.app, "GET", "/api/v1/admin/reports", Some(&mod_), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, _, notifs) =
        request(&env.app, "GET", "/api/v1/notifications", Some(&mod_), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(notifs
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "admin.report"));

    // Comment-length rejection against the config cap (2000).
    let too_long = "x".repeat(2100);
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/reports",
        Some(&alice),
        Some(json!({ "account_id": bob_id, "comment": too_long })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Plain users can't touch the admin surface.
    let (status, _, _) =
        request(&env.app, "GET", "/api/v1/admin/reports", Some(&alice), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn report_resolve_reopen_assign_and_audit() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let _admin = register(&env.app, "admin", "admin@x.test").await;
    make_role(&env, "admin", "admin").await;
    let admin = register(&env.app, "root", "root@x.test").await;
    make_role(&env, "root", "admin").await;
    let alice = register(&env.app, "alice", "alice@x.test").await;
    let _ = register(&env.app, "bob", "bob@x.test").await;
    let bob_id = account_id(&env, "bob").await;

    let (_, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/reports",
        Some(&alice),
        Some(json!({ "account_id": bob_id, "comment": "bad" })),
    )
    .await;
    let report_id = body["id"].as_str().unwrap().to_string();

    // assign_to_self then resolve.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/reports/{report_id}/assign_to_self"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/reports/{report_id}/resolve"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action_taken"], true);

    // Double resolve conflicts.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/reports/{report_id}/resolve"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Resolved reports leave the open queue, reopen puts them back.
    let (_, _, list) = request(&env.app, "GET", "/api/v1/admin/reports", Some(&admin), None).await;
    assert!(list.as_array().unwrap().is_empty());
    let (_, _, list) = request(
        &env.app,
        "GET",
        "/api/v1/admin/reports?resolved=true",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/reports/{report_id}/reopen"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Audit: assign + resolve + reopen = one row each.
    assert_eq!(audit_count(&env, "report.resolve").await, 1);
    assert_eq!(audit_count(&env, "report.reopen").await, 1);
    assert_eq!(audit_count(&env, "report.assign_to_self").await, 1);
}

// ---------------------------------------------------------------------------
// User actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn silence_and_suspend_effects() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "root", "root@x.test").await;
    make_role(&env, "root", "admin").await;
    let _alice = register(&env.app, "alice", "alice@x.test").await;
    let alice_id = account_id(&env, "alice").await;

    // Silence.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/action"),
        Some(&admin),
        Some(json!({ "type": "silence", "text": "borderline" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["limited"], true);
    assert_eq!(body["suspended"], false);

    // Suspend — the token dies with the suspension.
    let carol_token = register(&env.app, "carol", "carol@x.test").await;
    let carol_id = account_id(&env, "carol").await;
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{carol_id}/action"),
        Some(&admin),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["suspended"], true);
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&carol_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Actions + audit rows were written.
    let actions: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_actions")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(actions, 2);
    assert_eq!(audit_count(&env, "account.silence").await, 1);
    assert_eq!(audit_count(&env, "account.suspend").await, 1);

    // Unsuspend reverses the flag (access is not restored).
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{carol_id}/unsuspend"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["suspended"], false);
}

#[tokio::test]
async fn delete_is_admin_only_and_role_guard() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "root", "root@x.test").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    make_role(&env, "root", "admin").await;
    make_role(&env, "mod", "moderator").await;
    let _alice = register(&env.app, "alice", "alice@x.test").await;
    let alice_id = account_id(&env, "alice").await;

    // Moderators can't delete.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/action"),
        Some(&mod_),
        Some(json!({ "type": "delete" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Moderators can't change roles either (admin scope).
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/role"),
        Some(&mod_),
        Some(json!({ "role": "moderator" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin promotes alice, then last-admin demotion is refused.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/role"),
        Some(&admin),
        Some(json!({ "role": "moderator" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let root_id = account_id(&env, "root").await;
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{root_id}/role"),
        Some(&admin),
        Some(json!({ "role": "user" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(audit_count(&env, "account.role_change").await, 1);
}

// ---------------------------------------------------------------------------
// Domain blocks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn domain_block_crud_and_public_listing() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "root", "root@x.test").await;
    make_role(&env, "root", "admin").await;
    let alice = register(&env.app, "alice", "alice@x.test").await;

    // Normalization: URL-form domains collapse to the host.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&admin),
        Some(json!({
            "domain": "https://Spam.example:443/",
            "severity": "suspend",
            "private_comment": "spam farm",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["domain"], "spam.example");
    assert_eq!(body["severity"], "suspend");

    // Upsert on the same normalized domain updates, not duplicates.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&admin),
        Some(json!({
            "domain": "spam.example",
            "severity": "silence",
            "reject_media": true,
            "public_comment": "spammy",
            "obfuscate": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["severity"], "silence");
    assert_eq!(body["reject_media"], true);

    let (_, _, list) = request(
        &env.app,
        "GET",
        "/api/v1/admin/domain_blocks",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Public listing: obfuscated domain + public comment.
    let (status, _, public) = request(
        &env.app,
        "GET",
        "/api/v1/instance/domain_blocks",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{public}");
    let b = &public.as_array().unwrap()[0];
    assert!(b["domain"].as_str().unwrap().contains('*'));
    assert_eq!(b["comment"], "spammy");

    // Non-admin writes are refused.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&alice),
        Some(json!({ "domain": "x.test" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // IDN input stores the punycode form — the same serialization
    // `accounts.domain` holds for remote actors.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&admin),
        Some(json!({ "domain": "βόλοσ.example" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["domain"], "xn--nxasmq6b.example");

    // Blocking your own instance is refused (instance.domain defaults
    // to "localhost" in this env).
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/admin/domain_blocks",
        Some(&admin),
        Some(json!({ "domain": "https://Localhost:4000/x" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Delete lifts the block.
    let id = list[0]["id"].as_str().unwrap().to_string();
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/admin/domain_blocks/{id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(audit_count(&env, "domain_block.create").await, 2);
    assert_eq!(audit_count(&env, "domain_block.delete").await, 1);
}

#[tokio::test]
async fn public_listing_gated_by_config() {
    let mut config = Config::default();
    config.instance.expose_moderated_instances = false;
    let Some(env) = test_env(config).await else {
        return;
    };
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/instance/domain_blocks",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn action_guards_staff_and_self() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "root", "root@x.test").await;
    let mod_ = register(&env.app, "mod", "mod@x.test").await;
    let _mod2 = register(&env.app, "mod2", "mod2@x.test").await;
    make_role(&env, "root", "admin").await;
    make_role(&env, "mod", "moderator").await;
    make_role(&env, "mod2", "moderator").await;
    let root_id = account_id(&env, "root").await;
    let mod2_id = account_id(&env, "mod2").await;

    // Moderators can't act on staff (admins or fellow moderators).
    for target in [root_id, mod2_id] {
        let (status, _, _) = request(
            &env.app,
            "POST",
            &format!("/api/v1/admin/accounts/{target}/action"),
            Some(&mod_),
            Some(json!({ "type": "suspend" })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "moderator → staff target");
    }

    // Nobody can take an action on their own account.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{root_id}/action"),
        Some(&admin),
        Some(json!({ "type": "suspend" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "self action");

    // An admin can still act on another admin when one remains.
    let admin2 = register(&env.app, "root2", "root2@x.test").await;
    make_role(&env, "root2", "admin").await;
    let root2_id = account_id(&env, "root2").await;
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{root2_id}/action"),
        Some(&admin),
        Some(json!({ "type": "silence" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["limited"], true);
    let _ = admin2;
}

#[tokio::test]
async fn disable_freezes_and_enable_restores() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let admin = register(&env.app, "root", "root@x.test").await;
    make_role(&env, "root", "admin").await;
    let alice = register(&env.app, "alice", "alice@x.test").await;
    let alice_id = account_id(&env, "alice").await;

    // Disable — frozen: token revoked and rejected.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/action"),
        Some(&admin),
        Some(json!({ "type": "disable" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["disabled"], true);
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Enable lifts the flag; the revoked token stays dead.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/admin/accounts/{alice_id}/enable"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["disabled"], false);
    let (status, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked token stays dead");
}

// ---------------------------------------------------------------------------
// Inbound reports / audit job
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbound_flag_respects_reject_reports() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    // A remote target the Flag objects resolve to.
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('target.example', 'victim', 'https://target.example/users/victim', '', \
                 'https://target.example/inbox')",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    // The reporter's domain rejects reports (noop severity).
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, reject_reports) \
         VALUES ('bad.example', 'noop', true)",
    )
    .execute(&env.state.db)
    .await
    .unwrap();

    let core = env.state.ap.as_ref().unwrap().core.clone();
    let flag = |actor: &str| {
        let raw = json!({
            "type": "Flag",
            "id": format!("{actor}/activities/f1"),
            "actor": actor,
            "content": "abuse",
            "object": ["https://target.example/users/victim"],
        });
        (Activity::from_json(&raw), raw)
    };

    // Dropped silently — no report row.
    let (activity, raw) = flag("https://bad.example/users/r");
    activitypub::incoming::handle_flag(&core, &activity, &raw)
        .await
        .unwrap();
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM reports")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(reports, 0, "reject_reports domain must be dropped");

    // A normal domain files as usual.
    let (activity, raw) = flag("https://ok.example/users/r");
    activitypub::incoming::handle_flag(&core, &activity, &raw)
        .await
        .unwrap();
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM reports")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(reports, 1);
}

#[tokio::test]
async fn audit_cleanup_prunes_old_rows() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    sqlx::query(
        "INSERT INTO audit_log (action, target_type, created_at) \
         VALUES ('stale', 'account', now() - interval '400 days'), \
                ('fresh', 'account', now())",
    )
    .execute(&env.state.db)
    .await
    .unwrap();

    AuditLogCleanupJob
        .run(&job_ctx(&env), &json!({}))
        .await
        .unwrap();

    let remaining: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log ORDER BY action")
        .fetch_all(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, vec!["fresh".to_string()]);
}
