//! Epic-14 social-graph integration tests — follows, follow requests,
//! blocks, mutes, domain blocks, lists, notes, subscribe and counters,
//! against testcontainers Postgres+Redis.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
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
    social,
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
    Some(Env {
        app: build_router(state.clone()),
        state,
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
    let _permit = harness::acquire().await;
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
        Some(json!({"uri": format!("alice@{}", env.state.config().instance.domain)})),
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
        config: env.state.config().clone(),
        media: env.state.media.clone(),
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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

// ---------------------------------------------------------------------------
// Remote network listings (best-effort remote collection sync)
// ---------------------------------------------------------------------------

/// Loopback mock remote instance: actor docs under `/users/{name}`
/// (carrying `followers`/`following` collection URLs) and collections
/// under `/collections/{name}` — `carol-followers` embeds its first
/// page, `carol-following` links to a page URL (the one-hop `first`
/// fetch). Collection hits are counted so tests can prove reads are
/// served from the cache inside the sync TTL.
#[derive(Clone)]
struct MockRemote {
    base: String,
    hits: Arc<std::sync::atomic::AtomicUsize>,
}

async fn mock_actor_doc(
    axum::extract::State(m): axum::extract::State<MockRemote>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::Json<Value> {
    axum::Json(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/users/{name}", m.base),
        "type": "Person",
        "preferredUsername": name,
        "name": name,
        "inbox": format!("{}/users/{name}/inbox", m.base),
        "followers": format!("{}/collections/{name}-followers", m.base),
        "following": format!("{}/collections/{name}-following", m.base),
    }))
}

async fn mock_collection(
    axum::extract::State(m): axum::extract::State<MockRemote>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::Json<Value> {
    m.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let b = &m.base;
    let body = match name.as_str() {
        "carol-followers" => json!({
            "id": format!("{b}/collections/{name}"),
            "type": "OrderedCollection",
            "totalItems": 2,
            "first": {
                "type": "OrderedCollectionPage",
                "orderedItems": [
                    format!("{b}/users/dave"),
                    format!("{b}/users/erin"),
                ],
            },
        }),
        "carol-following" => json!({
            "id": format!("{b}/collections/{name}"),
            "type": "OrderedCollection",
            "totalItems": 1,
            "first": format!("{b}/collections/carol-following-page"),
        }),
        "carol-following-page" => json!({
            "id": format!("{b}/collections/{name}"),
            "type": "OrderedCollectionPage",
            "orderedItems": [format!("{b}/users/dave")],
        }),
        _ => json!({
            "id": format!("{b}/collections/{name}"),
            "type": "OrderedCollection",
            "totalItems": 0,
        }),
    };
    axum::Json(body)
}

async fn mock_remote() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://localhost:{port}");
    let state = MockRemote {
        base: base.clone(),
        hits: hits.clone(),
    };
    let app = axum::Router::new()
        .route("/users/{name}", axum::routing::get(mock_actor_doc))
        .route("/collections/{name}", axum::routing::get(mock_collection))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, hits)
}

/// A remote account seen for the first time (bare stub, no cached
/// actor document) gets its followers/following listings synced from
/// the remote collections on first read — members are materialized,
/// remote order is kept, and repeat reads inside the TTL don't
/// re-fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_network_listings_sync_from_remote_collections() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let (base, hits) = mock_remote().await;

    // First-seen remote account: a stub row with `actor_data = {}`,
    // the `upsert_remote_stub` shape — the sync must refetch the actor
    // document itself to learn the collection URLs.
    let carol = format!("{base}/users/carol");
    let carol_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('localhost', 'carol', $1, $1, '') RETURNING id",
    )
    .bind(&carol)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Followers — the embedded first page is materialized and remote
    // order is preserved.
    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{carol_id}/followers"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["username"].as_str())
        .collect();
    assert_eq!(names, ["dave", "erin"], "{list}");
    assert_eq!(list[0]["acct"], "dave@localhost");

    // The members materialized as accounts, and the edge cache holds
    // the remote ordering.
    let edges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM remote_network_edges \
         WHERE account_id = $1 AND direction = 'followers'",
    )
    .bind(carol_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(edges, 2);
    let member_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM accounts \
         WHERE domain = 'localhost' AND username IN ('dave', 'erin')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(member_rows, 2);

    // Following — `first` is a page URL, fetched one hop deeper.
    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{carol_id}/following"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["username"].as_str())
        .collect();
    assert_eq!(names, ["dave"], "{list}");

    // Repeat reads inside the TTL are served from the edge cache —
    // the claim row must stop remote re-fetches.
    let before = hits.load(std::sync::atomic::Ordering::SeqCst);
    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{carol_id}/followers"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "repeat read re-fetched remote collections"
    );
}

/// A remote account whose instance is unreachable degrades cleanly:
/// the endpoint still answers with whatever the local `follows` table
/// knows (here, nothing) instead of failing the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_network_listing_degrades_when_remote_is_down() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    // Port 1 is unreachable — every remote fetch fails fast.
    let ghost = "http://localhost:1/users/ghost";
    let ghost_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('localhost', 'ghost', $1, $1, '') RETURNING id",
    )
    .bind(ghost)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    let (code, list) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{ghost_id}/followers"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 0);
}
