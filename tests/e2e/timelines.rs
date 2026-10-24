//! Epic-15 timeline integration tests — home (Redis index + SQL),
//! local/federated public, list and tag timelines, pagination,
//! moderation filters, streaming events and the `timeline` rate
//! limit — against testcontainers Postgres+Redis.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    timelines::{self, fanout, jobs::TimelineFanOutJob},
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";

struct Env {
    app: Router,
    state: Arc<AppState>,
}

async fn test_env(config: Config) -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let mut config = config;
    let pool = harness::provision_db(infra, &slug).await;
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    Some(build_env(config, pool, redis_handle).await)
}

/// Config tweaks + `AppState`/AP wiring shared by `test_env` and the
/// outage test that brings its own killable Redis.
async fn build_env(mut config: Config, pool: PgPool, redis_handle: yonder::cache::Redis) -> Env {
    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();
    config.rate_limit.relation = config.rate_limit.authentication.clone();
    config.rate_limit.status = config.rate_limit.authentication.clone();
    config.rate_limit.media = config.rate_limit.authentication.clone();
    // Raised like the rest — unless the caller customized it (the
    // rate-limit test sets limit=1 and needs it to stick).
    if config.rate_limit.timeline.limit == 5 && config.rate_limit.timeline.period_seconds == 10 {
        config.rate_limit.timeline = config.rate_limit.authentication.clone();
    }
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-tl-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();

    let dispatcher = yonder::jobs::Dispatcher::new(&redis_handle, &config.jobs);
    let mut state = AppState::new(config, pool, redis_handle);
    let ap = activitypub::build(
        state.config().as_ref(),
        &state.db,
        &state.redis,
        &dispatcher,
        &state.bus,
    )
    .await
    .unwrap();
    state.ap = Some(ap);
    let state = Arc::new(state);
    Env {
        app: build_router(state.clone()),
        state,
    }
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
    let json = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, headers, json)
}

/// Post a status, return its entity id (string form).
async fn post(app: &Router, token: &str, body: Value) -> String {
    let mut payload = json!({"status": "hello"});
    if let Value::Object(m) = body {
        for (k, v) in m {
            payload[k] = v;
        }
    }
    let (status, _, entity) = request(
        &app.clone(),
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "post failed: {entity}");
    entity["id"].as_str().unwrap().to_string()
}

async fn timeline_ids(
    app: &Router,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<i64>) {
    let (status, headers, body) = request(app, "GET", path, token, None).await;
    let ids = body
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|s| s["id"].as_str().and_then(|i| i.parse::<i64>().ok()))
                .collect()
        })
        .unwrap_or_default();
    (status, headers, ids)
}

async fn account_id(db: &PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn user_id(db: &PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// A remote `accounts` row — the minimum a federated status needs.
async fn insert_remote_account(db: &PgPool, username: &str, domain: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO accounts (username, domain, actor_id) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(username)
    .bind(domain)
    .bind(format!("https://{domain}/users/{username}"))
    .fetch_one(db)
    .await
    .unwrap()
}

async fn insert_remote_status(
    db: &PgPool,
    account_id: i64,
    content: &str,
    visibility: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, visibility, local, uri) \
         VALUES ($1, $2, $3, false, $4) RETURNING id",
    )
    .bind(account_id)
    .bind(content)
    .bind(visibility)
    .bind(format!(
        "https://remote.example/statuses/{}",
        uuid::Uuid::new_v4()
    ))
    .fetch_one(db)
    .await
    .unwrap()
}

/// Run the fan-out job inline — the worker isn't started in tests.
async fn run_fanout(state: &AppState, status_id: i64) {
    let ctx = JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    };
    TimelineFanOutJob
        .run(&ctx, &json!({ "status_id": status_id }))
        .await
        .unwrap();
}

async fn run_backfill(state: &AppState, follower: i64, followee: i64) {
    let ctx = JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    };
    TimelineFanOutJob
        .run(&ctx, &json!({ "backfill_for": follower, "of": followee }))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Home timeline
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn home_shows_own_and_followed_newest_first() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let bob_id = account_id(&env.state.db, "bob").await;

    // Alice follows Bob (not Carol); Bob posts twice, Carol once.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let own = post(&env.app, &alice, json!({"status": "from alice"})).await;
    let b1 = post(&env.app, &bob, json!({"status": "b1"})).await;
    let b2 = post(&env.app, &bob, json!({"status": "b2"})).await;
    post(&env.app, &carol, json!({"status": "from carol"})).await;

    for id in [b1.parse::<i64>().unwrap(), b2.parse::<i64>().unwrap()] {
        run_fanout(&env.state, id).await;
    }
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    let expected: Vec<i64> = [b2, b1, own].iter().map(|s| s.parse().unwrap()).collect();
    assert_eq!(ids, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn home_paginates_with_link_headers() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            post(&env.app, &alice, json!({"status": format!("post {i}")}))
                .await
                .parse::<i64>()
                .unwrap(),
        );
    }
    // limit=2 → first page = newest two, Link rel=next + rel=prev.
    let (status, headers, page1) =
        timeline_ids(&env.app, "/api/v1/timelines/home?limit=2", Some(&alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1, vec![ids[3], ids[2]]);
    let link = headers
        .get(header::LINK)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(link.contains("rel=\"next\""), "missing next: {link}");
    assert!(link.contains("rel=\"prev\""), "missing prev: {link}");
    let max_id = link
        .split("max_id=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .and_then(|s| s.split('>').next())
        .unwrap()
        .to_string();
    let (_, _, page2) = timeline_ids(
        &env.app,
        &format!("/api/v1/timelines/home?limit=2&max_id={max_id}"),
        Some(&alice),
    )
    .await;
    assert_eq!(page2, vec![ids[1], ids[0]]);
    // min_id returns the immediately-newer page, still newest-first.
    let (_, _, newer) = timeline_ids(
        &env.app,
        &format!("/api/v1/timelines/home?limit=2&min_id={}", ids[0]),
        Some(&alice),
    )
    .await;
    assert_eq!(newer, vec![ids[2], ids[1]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn home_hides_boosts_when_show_reblogs_off() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let dave = register(&env.app, "dave", "d@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let bob_id = account_id(&env.state.db, "bob").await;

    let target = post(&env.app, &dave, json!({"status": "boost me"})).await;
    let boost_status = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{target}/reblog"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    assert_eq!(boost_status.0, StatusCode::OK);
    let boost_id: i64 = boost_status.2["id"].as_str().unwrap().parse().unwrap();

    // Alice follows Bob with boosts hidden; Dave follows Bob normally.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({"reblogs": false})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&dave),
        Some(json!({})),
    )
    .await;
    run_fanout(&env.state, boost_id).await;

    let (_, _, alice_ids) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    assert!(
        !alice_ids.contains(&boost_id),
        "alice must not see the boost"
    );
    let (_, _, dave_ids) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&dave)).await;
    assert!(dave_ids.contains(&boost_id), "dave sees the boost");
}

// ---------------------------------------------------------------------------
// Public timelines
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_local_and_federated_scopes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    post(&env.app, &alice, json!({"status": "local public"})).await;
    post(
        &env.app,
        &alice,
        json!({"status": "local unlisted", "visibility": "unlisted"}),
    )
    .await;
    let remote = insert_remote_account(&env.state.db, "eve", "remote.example").await;
    let remote_public =
        insert_remote_status(&env.state.db, remote, "remote public", "public").await;
    let remote_unlisted =
        insert_remote_status(&env.state.db, remote, "remote unlisted", "unlisted").await;
    let local_public: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE account_id = $1 AND visibility = 'public' ORDER BY id DESC LIMIT 1",
    )
    .bind(account_id(&env.state.db, "alice").await)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Federated (anonymous) — local + remote public only.
    let (status, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids.contains(&local_public) && ids.contains(&remote_public));
    assert!(!ids.contains(&remote_unlisted));

    // local=true — remote excluded.
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public?local=true", None).await;
    assert!(ids.contains(&local_public));
    assert!(!ids.contains(&remote_public));

    // remote=true — local excluded.
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public?remote=true", None).await;
    assert!(ids.contains(&remote_public));
    assert!(!ids.contains(&local_public));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moderation_filters_apply_to_viewer_and_scope() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    let bob_post = post(&env.app, &bob, json!({"status": "bob post"})).await;
    let bob_status_id: i64 = bob_post.parse().unwrap();

    // Mute hides the author for the muted viewer only.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/mute"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", Some(&alice)).await;
    assert!(!ids.contains(&bob_status_id), "muted author must be absent");
    // Anonymous viewers don't inherit user mutes.
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert!(ids.contains(&bob_status_id));

    // Unmute, block instead — blocks hide in both directions.
    request(
        &env.app,
        "DELETE",
        &format!("/api/v1/accounts/{bob_id}/mute"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/block"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", Some(&alice)).await;
    assert!(
        !ids.contains(&bob_status_id),
        "blocked author must be absent"
    );
    request(
        &env.app,
        "DELETE",
        &format!("/api/v1/accounts/{bob_id}/block"),
        Some(&alice),
        Some(json!({})),
    )
    .await;

    // Suspended authors vanish everywhere.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(bob_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert!(
        !ids.contains(&bob_status_id),
        "suspended author must be absent"
    );

    // Instance domain blocks (suspend) remove remote authors everywhere.
    let remote = insert_remote_account(&env.state.db, "spammer", "spam.example").await;
    let spam = insert_remote_status(&env.state.db, remote, "spam", "public").await;
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('spam.example','suspend')")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert!(
        !ids.contains(&spam),
        "domain-suspended author must be absent"
    );

    // A per-user domain block hides the domain for that user.
    let remote2 = insert_remote_account(&env.state.db, "ned", "ned.example").await;
    let ned_post = insert_remote_status(&env.state.db, remote2, "ned", "public").await;
    let alice_id = account_id(&env.state.db, "alice").await;
    sqlx::query(
        "INSERT INTO account_domain_blocks (account_id, domain) VALUES ($1, 'ned.example')",
    )
    .bind(alice_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", Some(&alice)).await;
    assert!(!ids.contains(&ned_post));
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert!(ids.contains(&ned_post));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_languages_hides_posts() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    post(
        &env.app,
        &bob,
        json!({"status": "bonjour", "language": "fr"}),
    )
    .await;
    let en = post(&env.app, &bob, json!({"status": "hello", "language": "en"})).await;
    let en_id: i64 = en.parse().unwrap();
    let alice_id = account_id(&env.state.db, "alice").await;
    sqlx::query("UPDATE users SET filter_languages = ARRAY['fr'] WHERE account_id = $1")
        .bind(alice_id)
        .execute(&env.state.db)
        .await
        .unwrap();

    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", Some(&alice)).await;
    assert_eq!(ids, vec![en_id]);
    // Anonymous viewers carry no language filter.
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert_eq!(ids.len(), 2);
}

// ---------------------------------------------------------------------------
// List & tag timelines
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_timeline_membership_policy_and_ownership() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let (bob_id, carol_id) = (
        account_id(&env.state.db, "bob").await,
        account_id(&env.state.db, "carol").await,
    );

    // Lists require membership follows.
    for target in [bob_id, carol_id] {
        request(
            &env.app,
            "POST",
            &format!("/api/v1/accounts/{target}/follow"),
            Some(&alice),
            Some(json!({})),
        )
        .await;
    }
    let (_, _, list) = request(
        &env.app,
        "POST",
        "/api/v1/lists",
        Some(&alice),
        Some(json!({"title": "crew", "replies_policy": "none"})),
    )
    .await;
    let list_id = list["id"].as_str().unwrap().to_string();
    request(
        &env.app,
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice),
        Some(json!({"account_ids": [bob_id.to_string()]})),
    )
    .await;

    let b1 = post(&env.app, &bob, json!({"status": "member post"})).await;
    post(&env.app, &carol, json!({"status": "non-member post"})).await;
    let reply = post(
        &env.app,
        &bob,
        json!({"status": "a reply", "in_reply_to_id": b1}),
    )
    .await;
    let reply_id: i64 = reply.parse().unwrap();
    let b1_id: i64 = b1.parse().unwrap();

    // members only; replies_policy=none hides the reply.
    let (status, _, ids) = timeline_ids(
        &env.app,
        &format!("/api/v1/timelines/list/{list_id}"),
        Some(&alice),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids, vec![b1_id], "expected member post only");
    assert!(!ids.contains(&reply_id));

    // Foreign list → 404.
    let (status, _, _) = timeline_ids(
        &env.app,
        &format!("/api/v1/timelines/list/{list_id}"),
        Some(&bob),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exclusive_list_removes_member_from_home() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let b1 = post(&env.app, &bob, json!({"status": "exclusive member"}))
        .await
        .parse::<i64>()
        .unwrap();

    let (_, _, list) = request(
        &env.app,
        "POST",
        "/api/v1/lists",
        Some(&alice),
        Some(json!({"title": "x", "exclusive": true})),
    )
    .await;
    let list_id = list["id"].as_str().unwrap().to_string();
    request(
        &env.app,
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice),
        Some(json!({"account_ids": [bob_id.to_string()]})),
    )
    .await;

    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    assert!(
        !ids.contains(&b1),
        "exclusive list member must not appear in home"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_timeline_matches_public_only() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let tagged = post(&env.app, &alice, json!({"status": "love #Rust"})).await;
    let tagged_id: i64 = tagged.parse().unwrap();
    post(&env.app, &alice, json!({"status": "untagged"})).await;
    post(
        &env.app,
        &alice,
        json!({"status": "quiet #rust", "visibility": "unlisted"}),
    )
    .await;

    let (status, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/tag/rust", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids, vec![tagged_id], "case-insensitive public match only");

    // local=true restricts to local authors.
    let remote = insert_remote_account(&env.state.db, "eve", "remote.example").await;
    let remote_tagged = insert_remote_status(&env.state.db, remote, "#rust remote", "public").await;
    sqlx::query(
        "INSERT INTO status_tags (status_id, tag_id) \
         SELECT $1, t.id FROM tags t WHERE lower(t.name) = 'rust'",
    )
    .bind(remote_tagged)
    .execute(&env.state.db)
    .await
    .unwrap();
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/tag/rust?local=true", None).await;
    assert_eq!(ids, vec![tagged_id]);
}

// ---------------------------------------------------------------------------
// Fan-out, fallback and streaming
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fanout_populates_index_and_trims() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (alice_id, bob_id) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;

    let s = post(&env.app, &bob, json!({"status": "fanned"}))
        .await
        .parse::<i64>()
        .unwrap();
    run_fanout(&env.state, s).await;
    let ids = fanout::index_ids(&env.state.redis, alice_id).await.unwrap();
    assert_eq!(ids, vec![s]);
    // The author's own feed gets it too.
    assert!(fanout::index_ids(&env.state.redis, bob_id)
        .await
        .unwrap()
        .contains(&s));

    // Trimming: lower the cap and fan a burst through `add` directly.
    let cfg = yonder::config::TimelinesConfig {
        home_max_entries: 3,
        ..Default::default()
    };
    let mut posted = Vec::new();
    for i in 0..5 {
        let id = post(&env.app, &bob, json!({"status": format!("burst {i}")}))
            .await
            .parse::<i64>()
            .unwrap();
        fanout::add(&env.state.db, &env.state.redis, &cfg, id, bob_id)
            .await
            .unwrap();
        posted.push(id);
    }
    let ids = fanout::index_ids(&env.state.redis, alice_id).await.unwrap();
    assert_eq!(ids.len(), 3, "index trimmed to cap: {ids:?}");
    assert_eq!(ids, posted[2..].iter().rev().cloned().collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_index_falls_back_to_sql_and_repopulates() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (alice_id, bob_id) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let s = post(&env.app, &bob, json!({"status": "sql only"}))
        .await
        .parse::<i64>()
        .unwrap();
    // No fan-out ran — the index is cold. Home still answers.
    assert!(fanout::index_ids(&env.state.redis, alice_id)
        .await
        .unwrap()
        .is_empty());
    let (_, _, ids) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    assert!(ids.contains(&s), "SQL fallback must cover a cold index");
    // …and the miss repopulated the index for next time.
    assert!(fanout::index_ids(&env.state.redis, alice_id)
        .await
        .unwrap()
        .contains(&s));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backfill_on_follow_seeds_index() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (alice_id, bob_id) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    let older = post(&env.app, &bob, json!({"status": "pre-follow"}))
        .await
        .parse::<i64>()
        .unwrap();
    // The REST follow enqueues the backfill job; run it inline.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    run_backfill(&env.state, alice_id, bob_id).await;
    assert_eq!(
        fanout::index_ids(&env.state.redis, alice_id).await.unwrap(),
        vec![older]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_receives_update_and_delete() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let mut rx = env.state.bus.subscribe();
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (alice_id, bob_id) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    let bob_uid = user_id(&env.state.db, bob_id).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        Some(json!({})),
    )
    .await;

    let id = post(&env.app, &alice, json!({"status": "stream me #rust"})).await;
    let mut streams = std::collections::HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "update" {
            streams.insert(ev.stream);
        }
    }
    assert!(streams.contains("public"), "public stream: {streams:?}");
    assert!(streams.contains("public:local"));
    assert!(streams.contains("hashtag:rust"));
    assert!(
        streams.contains(&format!("user:{bob_uid}")),
        "follower user stream: {streams:?}"
    );

    // Delete → `delete` on the same streams.
    request(
        &env.app,
        "DELETE",
        &format!("/api/v1/statuses/{id}"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let mut deleted = std::collections::HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "delete" {
            deleted.insert(ev.stream);
        }
    }
    assert!(deleted.contains("public"));
    assert!(deleted.contains(&format!("user:{bob_uid}")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeline_rate_limit_bucket() {
    let mut config = Config::default();
    config.rate_limit.timeline = RateLimit {
        limit: 1,
        period_seconds: 60,
        authenticated_limit: None,
    };
    let _permit = harness::acquire().await;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let (s1, _, _) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _, _) = timeline_ids(&env.app, "/api/v1/timelines/home", Some(&alice)).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
    // Anonymous hits the same bucket on the public timeline.
    let mut config = Config::default();
    config.rate_limit.timeline = RateLimit {
        limit: 1,
        period_seconds: 60,
        authenticated_limit: None,
    };
    let Some(env) = test_env(config).await else {
        return;
    };
    let (s1, _, _) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _, _) = timeline_ids(&env.app, "/api/v1/timelines/public", None).await;
    assert_eq!(s2, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redis_outage_falls_back_to_sql() {
    // A dedicated Redis we can kill mid-test — the shared container
    // must stay up for everyone else.
    let _permit = harness::acquire().await;
    let Some(infra) = harness::infra().await else {
        return;
    };
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let mut config = Config::default();
    let Some((redis_handle, kill_switch)) =
        harness::dedicated_redis(&mut config.redis, &slug).await
    else {
        return;
    };
    let env = build_env(config, pool, redis_handle).await;
    let alice = register(&env.app, "alice", "a@x.com").await;
    let alice_id = account_id(&env.state.db, "alice").await;
    let s = post(&env.app, &alice, json!({"status": "durable"}))
        .await
        .parse::<i64>()
        .unwrap();

    // Kill Redis — read_home must degrade to the SQL page, not 5xx.
    drop(kill_switch);
    let filters = timelines::Filters {
        viewer: Some(alice_id),
        ..Default::default()
    };
    let page = yonder::http::paginate::Page {
        max_id: None,
        since_id: None,
        limit: 20,
    };
    let rows = tokio::time::timeout(
        Duration::from_secs(30),
        fanout::read_home(
            &env.state.db,
            &env.state.redis,
            &env.state.config().timelines,
            &filters,
            &page,
        ),
    )
    .await
    .expect("read_home timed out on dead redis")
    .unwrap();
    assert!(rows.iter().any(|r| r.id == s));
}

// ---------------------------------------------------------------------------
// Review-fix regressions
// ---------------------------------------------------------------------------

/// `direct` statuses are DMs: they stream only to the mentioned local
/// users and the author — never to public, hashtag, list, or an
/// unmentioned follower's `user` stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_status_streams_only_to_recipients() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let mut rx = env.state.bus.subscribe();
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let alice_id = account_id(&env.state.db, "alice").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    let (alice_uid, bob_uid, carol_uid) = (
        user_id(&env.state.db, alice_id).await,
        user_id(&env.state.db, bob_id).await,
        user_id(&env.state.db, account_id(&env.state.db, "carol").await).await,
    );
    // Bob and Carol both follow Alice; only Bob is mentioned.
    for token in [&bob, &carol] {
        request(
            &env.app,
            "POST",
            &format!("/api/v1/accounts/{alice_id}/follow"),
            Some(token),
            Some(json!({})),
        )
        .await;
    }
    // Drain the follow-time events before the interesting write.
    while rx.try_recv().is_ok() {}

    let id = post(
        &env.app,
        &alice,
        json!({"status": "@bob secret", "visibility": "direct"}),
    )
    .await;
    let mut streams = std::collections::HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "update" {
            streams.insert(ev.stream);
        }
    }
    assert!(
        streams.contains(&format!("user:{bob_uid}")),
        "mentioned user stream: {streams:?}"
    );
    assert!(
        streams.contains(&format!("user:{alice_uid}")),
        "author's own stream: {streams:?}"
    );
    assert!(
        !streams.contains("public"),
        "DM leaked to public: {streams:?}"
    );
    assert!(
        !streams.contains(&format!("user:{carol_uid}")),
        "DM leaked to unmentioned follower: {streams:?}"
    );
    assert!(
        !streams
            .iter()
            .any(|s| s.starts_with("hashtag") || s.starts_with("list")),
        "DM leaked to tag/list stream: {streams:?}"
    );

    // Same audience for the `delete` event.
    request(
        &env.app,
        "DELETE",
        &format!("/api/v1/statuses/{id}"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    let mut deleted = std::collections::HashSet::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "delete" {
            deleted.insert(ev.stream);
        }
    }
    assert!(deleted.contains(&format!("user:{bob_uid}")));
    assert!(!deleted.contains("public"));
    assert!(!deleted.contains(&format!("user:{carol_uid}")));
}

/// A remote `Create` redelivered for an already-materialized object
/// must not re-publish `update` events (the upsert is idempotent; the
/// notification isn't, unless gated on "new row").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_create_redelivery_does_not_restream() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let mut rx = env.state.bus.subscribe();
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/tl1"),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": "https://remote.test/objects/tl1",
            "type": "Note",
            "attributedTo": "https://remote.test/users/eve",
            "content": "<p>hi from remote</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(yonder::posts::remote::handle(&core, &activity, &activity.to_json()).await);
    // The same Create delivered again (relay retry, second inbox).
    assert!(yonder::posts::remote::handle(&core, &activity, &activity.to_json()).await);

    let mut updates = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "update" {
            updates += 1;
        }
    }
    assert_eq!(updates, 1, "redelivered Create re-published `update`");
}

/// An existing index key with an empty *window* must not trigger a
/// rebuild — only a missing key (cold index) does. Detectable because
/// a rebuild would pull in statuses whose fan-out never ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_index_empty_window_not_repopulated() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let alice_id = account_id(&env.state.db, "alice").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    let s1 = post(&env.app, &alice, json!({"status": "s1"})).await;
    let s2 = post(&env.app, &alice, json!({"status": "s2"})).await;
    for id in [s1, s2].iter().map(|s| s.parse::<i64>().unwrap()) {
        run_fanout(&env.state, id).await;
    }
    // s3 is enqueued but never fanned out (no worker in tests) — a
    // repopulate would add it to the index.
    post(&env.app, &alice, json!({"status": "s3"})).await;

    let before = fanout::index_ids(&env.state.redis, bob_id).await.unwrap();
    assert_eq!(before.len(), 2);
    let filters = timelines::Filters {
        viewer: Some(bob_id),
        ..Default::default()
    };
    let page = yonder::http::paginate::Page {
        // Window strictly below the oldest member → empty.
        max_id: Some(*before.last().unwrap()),
        since_id: None,
        limit: 20,
    };
    let rows = fanout::read_home(
        &env.state.db,
        &env.state.redis,
        &env.state.config().timelines,
        &filters,
        &page,
    )
    .await
    .unwrap();
    assert!(rows.is_empty());
    let after = fanout::index_ids(&env.state.redis, bob_id).await.unwrap();
    assert_eq!(before, after, "empty window triggered a repopulate");
}

/// A *missing* key does repopulate — and the rebuild trims to
/// `home_max_entries`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_index_repopulates_and_trims() {
    let mut config = Config::default();
    config.timelines.home_max_entries = 5;
    let _permit = harness::acquire().await;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let alice_id = account_id(&env.state.db, "alice").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    let mut posted = Vec::new();
    for i in 0..8 {
        posted.push(
            post(&env.app, &alice, json!({"status": format!("s{i}")}))
                .await
                .parse::<i64>()
                .unwrap(),
        );
    }
    let filters = timelines::Filters {
        viewer: Some(bob_id),
        ..Default::default()
    };
    let page = yonder::http::paginate::Page {
        max_id: None,
        since_id: None,
        limit: 20,
    };
    let rows = fanout::read_home(
        &env.state.db,
        &env.state.redis,
        &env.state.config().timelines,
        &filters,
        &page,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 8);
    let index = fanout::index_ids(&env.state.redis, bob_id).await.unwrap();
    assert_eq!(index.len(), 5, "rebuild must trim to home_max_entries");
    assert_eq!(index[0], *posted.last().unwrap(), "newest first");
}
