//! Epic-14 social-graph integration tests — follows, follow requests,
//! blocks, mutes, domain blocks, lists, notes, subscribe and counters,
//! against testcontainers Postgres+Redis.
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
    activitypub, cache,
    config::{Config, RateLimit},
    db,
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    social,
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
    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();
    config.rate_limit.relation = config.rate_limit.authentication.clone();
    config.rate_limit.status = config.rate_limit.authentication.clone();
    config.rate_limit.media = config.rate_limit.authentication.clone();
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-int-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();

    let dispatcher = yonder::jobs::Dispatcher::new(&redis_handle, &config.jobs);
    let ap = activitypub::build(&config, &pool, &redis_handle, &dispatcher)
        .await
        .unwrap();
    let mut state = AppState::new(config, pool, redis_handle);
    state.ap = Some(ap);
    let state = Arc::new(state);
    Some(Env {
        app: build_router(state.clone()),
        state,
        _pg: pg,
        _redis: redis,
    })
}

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
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
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
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

async fn account_id(db: &PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn counts(db: &PgPool, id: i64) -> (i32, i32) {
    sqlx::query_as::<_, (i32, i32)>(
        "SELECT followers_count, following_count FROM accounts WHERE id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap()
}

/// `NotificationFanout` event kinds on the default queue.
async fn fanout_kinds(env: &Env) -> Vec<String> {
    let key = env.state.redis.key("queue:default");
    let mut conn = env
        .state
        .redis
        .client()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let raw: Vec<(String, Vec<String>)> = redis::cmd("XRANGE")
        .arg(&key)
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    raw.iter()
        .flat_map(|(_, fields)| {
            fields
                .chunks(2)
                .filter(|pair| pair[0] == "job")
                .map(|pair| pair[1].clone())
                .collect::<Vec<_>>()
        })
        .filter_map(|job| serde_json::from_str::<Value>(&job).ok())
        .filter(|env| env["kind"] == "notification_fanout")
        .filter_map(|env| env["payload"]["type"].as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_roundtrip_flags_and_unfollow() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (a, b) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );

    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["following"], true);
    assert_eq!(rel["showing_reblogs"], true);
    assert_eq!(rel["notifying"], false);
    assert_eq!(rel["requested"], false);
    assert_eq!(counts(&env.state.db, b).await, (1, 0));
    assert_eq!(counts(&env.state.db, a).await, (0, 1));

    // A `follow` notification event fired.
    assert!(fanout_kinds(&env).await.contains(&"follow".to_string()));

    // Re-follow reconciles flags without a second row.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        Some(json!({"reblogs": false, "notify": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["showing_reblogs"], false);
    assert_eq!(rel["notifying"], true);
    assert_eq!(counts(&env.state.db, b).await, (1, 0));

    // Self-follow and unknown target reject.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    let (code, _) = request(
        &env.app,
        "POST",
        "/api/v1/accounts/99999/follow",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Batch relationships view — bob's view of alice: she follows him.
    let (code, rels) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={a}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rels[0]["followed_by"], true);

    // Listings.
    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{b}/followers"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list[0]["username"], "alice");
    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{a}/following"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list[0]["username"], "bob");

    // Deprecated POST /api/v1/follows via acct URI (local target).
    let (code, acct) = request(
        &env.app,
        "POST",
        "/api/v1/follows",
        Some(&bob),
        Some(json!({"uri": format!("alice@{}", env.state.config.instance.domain)})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{acct}");
    assert_eq!(acct["username"], "alice");

    // Unfollow — counters decrement, idempotent.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/unfollow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel["following"], false);
    assert_eq!(counts(&env.state.db, b).await, (0, 1)); // bob still follows alice
    assert_eq!(counts(&env.state.db, a).await, (1, 0));
    let (code, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(counts(&env.state.db, a).await, (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_follow_request_authorize_and_reject() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let (a, b, c) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
        account_id(&env.state.db, "carol").await,
    );
    // Bob locks his account.
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();

    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["following"], false);
    assert_eq!(rel["requested"], true);
    assert!(fanout_kinds(&env)
        .await
        .contains(&"follow_request".to_string()));

    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel["requested"], true);

    // Bob sees both pending requesters.
    let (code, list) = request(&env.app, "GET", "/api/v1/follow_requests", Some(&bob), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // Authorize alice → she follows him + counters + `follow` event.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/follow_requests/{a}/authorize"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["followed_by"], true);
    assert_eq!(counts(&env.state.db, b).await, (1, 0));
    assert!(fanout_kinds(&env).await.contains(&"follow".to_string()));

    // Reject carol → row gone, no follow.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/follow_requests/{c}/reject"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["following"], false);
    assert_eq!(rel["requested_by"], false);
    let (code, list) = request(&env.app, "GET", "/api/v1/follow_requests", Some(&bob), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 0);

    // Authorizing a nonexistent request 404s.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/follow_requests/{c}/authorize"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_severs_and_refollow_fails() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (a, b) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    // Mutual follows first.
    for (t, target) in [(&alice, b), (&bob, a)] {
        let (code, _) = request(
            &env.app,
            "POST",
            &format!("/api/v1/accounts/{target}/follow"),
            Some(t),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }

    // unfollow_blocked=true: block removes follows both directions.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/block"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["blocking"], true);
    assert_eq!(rel["following"], false);
    assert_eq!(counts(&env.state.db, b).await, (0, 0));
    assert_eq!(counts(&env.state.db, a).await, (0, 0));

    // Blocked pair can't follow again — either direction.
    for (t, target) in [(&alice, b), (&bob, a)] {
        let (code, _) = request(
            &env.app,
            "POST",
            &format!("/api/v1/accounts/{target}/follow"),
            Some(t),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // /blocks lists the block; unblock clears it.
    let (code, list) = request(&env.app, "GET", "/api/v1/blocks", Some(&alice), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list[0]["username"], "bob");
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/unblock"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel["blocking"], false);
    // Follows do not resurrect.
    assert_eq!(counts(&env.state.db, b).await, (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_domain_and_notes() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let b = account_id(&env.state.db, "bob").await;

    // Mute with duration → expires_at set, muting flags on.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/mute"),
        Some(&alice),
        Some(json!({"notifications": false, "duration": 3600})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["muting"], true);
    assert_eq!(rel["muting_notifications"], false);
    let expires: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT expires_at FROM mutes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(account_id(&env.state.db, "alice").await)
    .bind(b)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(expires.unwrap() > chrono::Utc::now());

    let (code, list) = request(&env.app, "GET", "/api/v1/mutes", Some(&alice), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list[0]["username"], "bob");

    // Expired mute drops the flag.
    sqlx::query("UPDATE mutes SET expires_at = now() - interval '1 hour'")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (code, rel) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={b}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(rel[0]["muting"], false);
    let _ = code;

    // Unmute removes the row.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/unmute"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Domain blocks round-trip.
    let (code, _) = request(
        &env.app,
        "POST",
        "/api/v1/domain_blocks",
        Some(&alice),
        Some(json!({"domain": "bad.example"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, list) = request(&env.app, "GET", "/api/v1/domain_blocks", Some(&alice), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list[0], "bad.example");
    let (code, _) = request(
        &env.app,
        "DELETE",
        "/api/v1/domain_blocks?domain=bad.example",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // User note: visible only to the author.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/note"),
        Some(&alice),
        Some(json!({"comment": "met at fosdem"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["note"], "met at fosdem");
    let (code, rel) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={b}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel[0]["note"], "");
    // Too-long note 422s; empty clears.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/note"),
        Some(&alice),
        Some(json!({"comment": "x".repeat(2001)})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/note"),
        Some(&alice),
        Some(json!({"comment": ""})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel["note"], "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_crud_and_membership() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let b = account_id(&env.state.db, "bob").await;

    let (code, list) = request(
        &env.app,
        "POST",
        "/api/v1/lists",
        Some(&alice),
        Some(json!({"title": "friends", "replies_policy": "list", "exclusive": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(list["title"], "friends");
    assert_eq!(list["exclusive"], true);
    let list_id: i64 = list["id"].as_str().unwrap().parse().unwrap();

    // Invalid policy 422s.
    let (code, _) = request(
        &env.app,
        "POST",
        "/api/v1/lists",
        Some(&alice),
        Some(json!({"title": "x", "replies_policy": "bogus"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Members add/remove.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice),
        Some(json!({"account_ids": [b]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, members) = request(
        &env.app,
        "GET",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(members[0]["username"], "bob");
    // accounts/:id/lists reflects membership.
    let (code, ls) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{b}/lists"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(ls.as_array().unwrap().len(), 1);

    // Update title; foreign list access is 404.
    let (code, upd) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/lists/{list_id}"),
        Some(&alice),
        Some(json!({"title": "close friends"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(upd["title"], "close friends");
    let (code, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/lists/{list_id}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Remove member, delete list.
    let (code, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/lists/{list_id}/accounts"),
        Some(&alice),
        Some(json!({"account_ids": [b]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/lists/{list_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_and_counters() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let _bob = register(&env.app, "bob", "b@x.com").await;
    let (a, b) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );

    // Subscribe without following.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/subscribe"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["subscribing"], true);
    assert_eq!(rel["following"], false);
    let (code, rel) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/accounts/{b}/subscribe"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel["subscribing"], false);

    // Triggers: a raw follows insert/delete moves local counts.
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(a)
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();
    assert_eq!(counts(&env.state.db, b).await, (1, 0));
    sqlx::query("DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2")
        .bind(a)
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();
    assert_eq!(counts(&env.state.db, b).await, (0, 0));

    // Drift then recount() repairs.
    sqlx::query("UPDATE accounts SET followers_count = 42 WHERE id = $1")
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();
    assert!(social::counters::recount(&env.state.db, b).await.unwrap());
    assert_eq!(counts(&env.state.db, b).await, (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_follow_and_block_arms() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let _ = alice;
    let a = account_id(&env.state.db, "alice").await;
    let alice_actor: String = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(a)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";

    // Inbound Follow → PassThrough + a delayed FollowEvent job, through
    // the real `on_activity` hook wiring (not the inner handler).
    let hook = activitypub::incoming::on_activity_fn(&core);
    let raw = json!({
        "type": "Follow", "actor": remote_actor, "object": alice_actor,
    });
    let act = pubby_sdk::Activity {
        id: format!("{remote_actor}/activities/f1"),
        activity_type: "Follow".into(),
        actor: remote_actor.into(),
        object: raw["object"].clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    let disp = hook(&act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::PassThrough));

    // Simulate pubby's store: remote row + follows row, then the job
    // emits `follow`.
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('remote.test', 'eve', $1, $1, 'https://remote.test/inbox')",
    )
    .bind(remote_actor)
    .execute(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id) \
         SELECT r.id, $2 FROM accounts r WHERE r.actor_id = $1",
    )
    .bind(remote_actor)
    .bind(a)
    .execute(&env.state.db)
    .await
    .unwrap();
    // Trigger counted the remote follower on the local account.
    assert_eq!(counts(&env.state.db, a).await, (1, 0));

    let ctx = JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config.clone(),
    };
    social::jobs::FollowEventJob
        .run(
            &ctx,
            &json!({"actor_id": remote_actor, "target_actor_id": alice_actor}),
        )
        .await
        .unwrap();
    assert!(fanout_kinds(&env).await.contains(&"follow".to_string()));

    // Inbound Block → audit row + sever the local follow.
    let (act, raw) = (
        pubby_sdk::Activity {
            id: format!("{remote_actor}/activities/b1"),
            activity_type: "Block".into(),
            actor: remote_actor.into(),
            object: json!(alice_actor),
            to: vec![],
            cc: vec![],
            published: None,
            signature: None,
        },
        json!({"type": "Block", "actor": remote_actor, "object": alice_actor}),
    );
    let disp = hook(&act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::Handled));
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM follows f JOIN accounts r ON r.id = f.account_id \
         WHERE r.actor_id = $1 AND f.target_account_id = $2",
    )
    .bind(remote_actor)
    .bind(a)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    assert_eq!(counts(&env.state.db, a).await, (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_block_mute_zero_duration_and_blockers_visible() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let b = account_id(&env.state.db, "bob").await;

    // Self-block → 422.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/block"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // `duration: 0` means indefinite — expires_at stays NULL, still muting.
    let (code, rel) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/mute"),
        Some(&alice),
        Some(json!({"duration": 0})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rel}");
    assert_eq!(rel["muting"], true);
    let expires: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT expires_at FROM mutes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(expires.is_none());

    // `blockers_visible` defaults false → `blocked_by` is masked even
    // though bob really blocked alice.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/block"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, rel) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={b}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel[0]["blocked_by"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blockers_visible_shows_blocked_by_and_delete_cascades() {
    let mut config = Config::default();
    config.admin.blockers_visible = true;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let b = account_id(&env.state.db, "bob").await;

    // `blockers_visible = true` → the blocked user sees `blocked_by`.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/block"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, rel) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/relationships?id[]={b}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rel[0]["blocked_by"], true);

    // Bob still blocks alice → refollow is rejected; drop bob's block
    // so the follow can land.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/unblock"),
        Some(&bob),
        None,
    )
    .await;
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(counts(&env.state.db, a).await, (1, 1));
    assert_eq!(counts(&env.state.db, b).await, (1, 1));

    // Deleting bob cascades his follows rows; the DELETE trigger
    // updates alice's (live) counts and must not error on bob's row.
    sqlx::query("DELETE FROM accounts WHERE id = $1")
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();
    assert_eq!(counts(&env.state.db, a).await, (0, 0));
    let orphan: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM follows WHERE account_id = $1 OR target_account_id = $1",
    )
    .bind(b)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(orphan, 0);
}
