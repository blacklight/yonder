//! Epic-16 notification integration tests — generation per type,
//! dedupe, suppression filters, the REST surface, the Redis-relayed
//! WebSocket push and email enqueueing — against testcontainers
//! Postgres+Redis.
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
use redis::AsyncCommands;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, ws::StreamEvent, AppState},
    jobs::{queue, JobContext, JobEnvelope, JobHandler, JobKind},
    notifications::{self, fanout::NotificationFanoutJob},
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
        .join(format!("yonder-nt-{}", uuid::Uuid::new_v4()))
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

async fn post(app: &Router, token: &str, body: Value) -> String {
    let mut payload = json!({"status": "hello"});
    if let Value::Object(m) = body {
        for (k, v) in m {
            payload[k] = v;
        }
    }
    let (status, _, entity) =
        request(app, "POST", "/api/v1/statuses", Some(token), Some(payload)).await;
    assert_eq!(status, StatusCode::OK, "post failed: {entity}");
    entity["id"].as_str().unwrap().to_string()
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

fn job_ctx(env: &Env) -> JobContext {
    JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    }
}

async fn run_fanout(env: &Env, payload: Value) {
    NotificationFanoutJob
        .run(&job_ctx(env), &payload)
        .await
        .unwrap();
}

/// Drain every pending entry from the `default` job stream and return
/// the decoded envelopes — the test stands in for the worker.
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

/// Run every pending `NotificationFanout` job; returns the envelopes
/// the runs produced (e.g. `EmailSend`) plus anything non-fanout that
/// was already queued.
async fn run_pending_fanouts(env: &Env) -> Vec<JobEnvelope> {
    let mut rest = Vec::new();
    for envelope in drain_default_queue(env).await {
        if envelope.kind == JobKind::NotificationFanout {
            NotificationFanoutJob
                .run(&job_ctx(env), &envelope.payload)
                .await
                .unwrap();
        } else {
            rest.push(envelope);
        }
    }
    // Jobs the fanouts just produced (emails) land after the first
    // drain — drain once more.
    rest.extend(drain_default_queue(env).await);
    rest
}

/// Notification rows for `account_id`, `(type, from_account_id)` pairs.
async fn rows_for(db: &PgPool, account_id: i64) -> Vec<(String, i64)> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT type, from_account_id FROM notifications WHERE account_id = $1 ORDER BY id",
    )
    .bind(account_id)
    .fetch_all(db)
    .await
    .unwrap()
}

async fn notification_count(db: &PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Start the Redis→bus relay (the serve path spawns it; tests spawn it
/// per env) and wait until it is actually subscribed — pub/sub drops
/// messages sent before the subscription lands.
async fn start_relay(
    env: &Env,
) -> (
    CancellationToken,
    tokio::sync::broadcast::Receiver<StreamEvent>,
) {
    let cancel = CancellationToken::new();
    tokio::spawn(yonder::http::ws::relay::run(
        env.state.redis.clone(),
        env.state.bus.clone(),
        cancel.clone(),
    ));
    let mut rx = env.state.bus.subscribe();
    for _ in 0..100 {
        env.state
            .redis
            .publish(
                &env.state.redis.key("stream"),
                &json!({"stream": "__probe__", "event": "probe", "payload": null}),
            )
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Ok(ev)) if ev.stream == "__probe__" => return (cancel, rx),
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("stream relay did not come up");
}

/// Collect bus events until `want` events are buffered or 5s pass.
async fn collect(
    rx: &mut tokio::sync::broadcast::Receiver<StreamEvent>,
    want: usize,
) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => out.push(ev),
            _ => break,
        }
        if out.len() >= want {
            // A short drain so a late duplicate still counts.
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn favourite_reblog_quote_generate_notifications() {
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

    let id = post(&env.app, &alice, json!({"status": "react to me"})).await;

    // Favourite + reblog (Bob → Alice's post).
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    // Quote (Bob quoting Alice's post).
    post(
        &env.app,
        &bob,
        json!({"status": "check this", "quote_id": id}),
    )
    .await;

    run_pending_fanouts(&env).await;
    let rows = rows_for(&env.state.db, a).await;
    assert_eq!(
        rows,
        vec![
            ("favourite".to_string(), b),
            ("reblog".to_string(), b),
            ("quote".to_string(), b),
        ]
    );
    // Bob (the actor) never self-notifies.
    assert_eq!(notification_count(&env.state.db, b).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_and_follow_request_notify() {
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
    // Locked account → follow_request instead of follow.
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(b)
        .execute(&env.state.db)
        .await
        .unwrap();

    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/follow"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;

    assert_eq!(
        rows_for(&env.state.db, a).await,
        vec![("follow".to_string(), b)]
    );
    assert_eq!(
        rows_for(&env.state.db, b).await,
        vec![("follow_request".to_string(), a)]
    );

    // Accepting the request converts it to a `follow` notification
    // (authorize takes the requester's account id).
    request(
        &env.app,
        "POST",
        &format!("/api/v1/follow_requests/{a}/authorize"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(
        rows_for(&env.state.db, b).await,
        vec![("follow_request".to_string(), a), ("follow".to_string(), a)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mention_and_status_notifications() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let (a, b, c) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
        account_id(&env.state.db, "carol").await,
    );

    // Carol follows Alice with notify → `status` notifications.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&carol),
        Some(json!({"notify": true})),
    )
    .await;
    run_pending_fanouts(&env).await; // consume the follow event

    post(&env.app, &alice, json!({"status": "hey @bob, hi"})).await;
    run_pending_fanouts(&env).await;

    assert_eq!(
        rows_for(&env.state.db, b).await,
        vec![("mention".to_string(), a)]
    );
    assert_eq!(
        rows_for(&env.state.db, c).await,
        vec![("status".to_string(), a)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bookmark_and_self_interaction_never_notify() {
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

    let id = post(&env.app, &alice, json!({"status": "save me"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/bookmark"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    // Self-favourite emits nothing.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);
    assert_eq!(notification_count(&env.state.db, b).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_interaction_payload_shape() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let remote = insert_remote_account(&env.state.db, "eve", "remote.example").await;
    // A local status by Alice that Eve (remote) liked.
    let id = post(&env.app, &alice, json!({"status": "local post"})).await;
    let status_uri: String = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();

    // The pubby `on_interaction_received` shape (URIs, not ids).
    run_fanout(
        &env,
        json!({
            "source_actor_id": "https://remote.example/users/eve",
            "target_resource": status_uri,
            "interaction_type": "like",
            "object_id": status_uri,
            "activity_id": "https://remote.example/activities/1",
        }),
    )
    .await;
    assert_eq!(
        rows_for(&env.state.db, a).await,
        vec![("favourite".to_string(), remote)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedupe_collapses_repeats() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (a, _b) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
    );
    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    let envelopes = drain_default_queue(&env).await;
    let payload = envelopes
        .iter()
        .find(|e| e.kind == JobKind::NotificationFanout)
        .unwrap()
        .payload
        .clone();
    // The same job delivered twice (at-least-once) → one row.
    run_fanout(&env, payload.clone()).await;
    run_fanout(&env, payload).await;
    assert_eq!(notification_count(&env.state.db, a).await, 1);
}

// ---------------------------------------------------------------------------
// Suppression filters
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocks_suppress_in_both_directions() {
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
    let id = post(&env.app, &alice, json!({"status": "x"}))
        .await
        .parse::<i64>()
        .unwrap();
    let payload = json!({
        "type": "favourite",
        "from_account_id": b,
        "status_id": id,
        "account_id": a,
    });

    // Alice blocks Bob → his event can't reach her.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/block"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    run_fanout(&env, payload.clone()).await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/unblock"),
        Some(&alice),
        Some(json!({})),
    )
    .await;

    // The other direction: Bob blocks Alice — her notifications from
    // him are suppressed too.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/block"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_fanout(&env, payload).await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_with_hide_notifications_suppresses() {
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
    let id = post(&env.app, &alice, json!({"status": "x"})).await;

    // notifications: true → hide_notifications → suppressed.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/mute"),
        Some(&alice),
        Some(json!({"notifications": true})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);

    // notifications: false → the mute doesn't reach notifications.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/unmute"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{b}/mute"),
        Some(&alice),
        Some(json!({"notifications": false})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unfavourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(notification_count(&env.state.db, a).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn domain_block_and_stranger_filter_suppress() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let eve = insert_remote_account(&env.state.db, "eve", "blocked.example").await;
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, visibility, local, uri) \
         VALUES ($1, 'x', 'public', true, $2) RETURNING id",
    )
    .bind(a)
    .bind(format!("{BASE}/statuses/d1"))
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Alice domain-blocks the sender's instance.
    request(
        &env.app,
        "POST",
        "/api/v1/domain_blocks",
        Some(&alice),
        Some(json!({"domain": "blocked.example"})),
    )
    .await;
    run_fanout(
        &env,
        json!({
            "type": "favourite",
            "from_account_id": eve,
            "status_id": status_id,
            "account_id": a,
        }),
    )
    .await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);
    request(
        &env.app,
        "DELETE",
        "/api/v1/domain_blocks?domain=blocked.example",
        Some(&alice),
        Some(json!({})),
    )
    .await;

    // `block_stranger_notifications` on → same sender suppressed.
    sqlx::query("UPDATE users SET block_stranger_notifications = true WHERE account_id = $1")
        .bind(a)
        .execute(&env.state.db)
        .await
        .unwrap();
    run_fanout(
        &env,
        json!({
            "type": "favourite",
            "from_account_id": eve,
            "status_id": status_id,
            "account_id": a,
        }),
    )
    .await;
    assert_eq!(notification_count(&env.state.db, a).await, 0);

    // Once Alice follows Eve she stops being a stranger.
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(a)
        .bind(eve)
        .execute(&env.state.db)
        .await
        .unwrap();
    run_fanout(
        &env,
        json!({
            "type": "favourite",
            "from_account_id": eve,
            "status_id": status_id,
            "account_id": a,
        }),
    )
    .await;
    assert_eq!(notification_count(&env.state.db, a).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_report_notifies_staff_only() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env.app, "alice", "a@x.com").await;
    register(&env.app, "mod", "m@x.com").await;
    register(&env.app, "plain", "p@x.com").await;
    let (a, m, p) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "mod").await,
        account_id(&env.state.db, "plain").await,
    );
    sqlx::query("UPDATE accounts SET role = 'moderator' WHERE id = $1")
        .bind(m)
        .execute(&env.state.db)
        .await
        .unwrap();
    let report_id: i64 = sqlx::query_scalar(
        "INSERT INTO reports (account_id, target_account_id, category, comment) \
         VALUES ($1, $2, 'spam', 'spammy') RETURNING id",
    )
    .bind(a)
    .bind(p)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    notifications::notify_report(&env.state.db, &env.state.dispatcher, report_id)
        .await
        .unwrap();
    run_pending_fanouts(&env).await;
    assert_eq!(
        rows_for(&env.state.db, m).await,
        vec![("admin.report".to_string(), a)]
    );
    // The reporter and regular users get nothing.
    assert_eq!(notification_count(&env.state.db, a).await, 0);
    assert_eq!(notification_count(&env.state.db, p).await, 0);
    // The row links back to the report for the entity's `report` field.
    let linked: Option<i64> =
        sqlx::query_scalar("SELECT report_id FROM notifications WHERE account_id = $1")
            .bind(m)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(linked, Some(report_id));
}

// ---------------------------------------------------------------------------
// REST surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_list_filters_paginate_clear_dismiss() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;

    // List all.
    let (status, _, body) =
        request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    let all = body.as_array().unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0]["type"], "follow"); // newest first
    assert!(all[0]["account"]["id"].is_string());

    // Mastodon filter.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications?types[]=favourite&types[]=reblog",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications?exclude_types[]=favourite&exclude_types[]=reblog",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Yonder aliases.
    for (alias, len) in [("likes", 1), ("boosts", 1), ("all", 3)] {
        let (_, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/notifications?types[]={alias}"),
            Some(&alice),
            None,
        )
        .await;
        assert_eq!(
            body.as_array().unwrap().len(),
            len,
            "alias {alias} should match {len}"
        );
    }
    // Unknown filter values are accepted, Mastodon-style — newer
    // clients enumerate types the server never emits
    // (`severed_relationships`, `reaction`, …): `types[]` selects no
    // rows, `exclude_types[]` excludes nothing.
    for (query, len) in [
        ("types[]=bogus", 0),
        ("types[]=severed_relationships", 0),
        ("exclude_types[]=severed_relationships", 3),
        ("exclude_types[]=reaction", 3),
    ] {
        let (status, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/notifications?{query}"),
            Some(&alice),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "query {query} should not 422");
        assert_eq!(
            body.as_array().unwrap().len(),
            len,
            "query {query} should match {len}"
        );
    }

    // Pagination: limit=1 pages via max_id + Link header.
    let (status, headers, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications?limit=1",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    let link = headers
        .get(header::LINK)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(link.contains("max_id"));
    let next = link
        .split("max_id=")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap()
        .to_string();
    let (_, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/notifications?max_id={next}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);

    // GET /:id — own works, foreign is 404.
    let nid = all[0]["id"].as_str().unwrap();
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/notifications/{nid}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], nid);
    let (status, _, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/notifications/{nid}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // unread_count, dismiss, clear.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["count"], 3);
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/notifications/{nid}/dismiss"),
        Some(&alice),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["count"], 2);
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/notifications/clear",
        Some(&alice),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_read_updates_unread_count_but_keeps_rows() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;

    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;

    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    let nid = body[0]["id"].as_str().unwrap().to_string();
    assert_eq!(body[0]["read"], false);
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["count"], 1);

    // The entity's wire form is a string — snowflake ids exceed JS's
    // safe-integer range, so the SPA posts them verbatim as strings.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/notifications/read",
        Some(&alice),
        Some(json!({ "ids": [nid] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["count"], 0);
    // Read ≠ dismissed — the row stays in the list, now flagged read.
    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["read"], true);

    // Bare JSON numbers are accepted too — a second notification
    // marked by its numeric form flips as well.
    let id = post(&env.app, &alice, json!({"status": "y"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    let nid2 = body[0]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/notifications/read",
        Some(&alice),
        Some(json!({ "ids": [nid2] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["count"], 0);

    // Malformed ids are a validation error, not a silent miss.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/notifications/read",
        Some(&alice),
        Some(json!({ "ids": ["not-a-number"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Live push
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_push_via_redis_relay() {
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
    let (a_uid, b_uid) = (
        user_id(&env.state.db, a).await,
        user_id(&env.state.db, b).await,
    );
    let (cancel, mut rx) = start_relay(&env).await;
    while rx.try_recv().is_ok() {} // drain probe leftovers

    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    // Drive the job exactly like the worker would.
    for envelope in drain_default_queue(&env).await {
        if envelope.kind == JobKind::NotificationFanout {
            NotificationFanoutJob
                .run(&job_ctx(&env), &envelope.payload)
                .await
                .unwrap();
        }
    }

    let events = collect(&mut rx, 3).await;
    let notification_streams: Vec<&str> = events
        .iter()
        .filter(|e| e.event == "notification")
        .map(|e| e.stream.as_str())
        .collect();
    assert!(
        notification_streams.contains(&format!("user:notification:{a_uid}").as_str()),
        "expected user:notification:{a_uid}, got {notification_streams:?}"
    );
    assert!(notification_streams.contains(&format!("user:{a_uid}").as_str()));
    // Never to another user's scoped stream.
    assert!(!notification_streams.contains(&format!("user:notification:{b_uid}").as_str()));
    let count = events.iter().find(|e| e.event == "notifications_count");
    assert!(count.is_some(), "count event missing: {events:?}");
    assert_eq!(count.unwrap().payload["unread_count"], 1);
    // The entity rides inside the notification event.
    let notif = events
        .iter()
        .find(|e| e.event == "notification" && e.stream == format!("user:notification:{a_uid}"))
        .unwrap();
    assert_eq!(notif.payload["type"], "favourite");
    assert_eq!(notif.payload["account"]["username"], "bob");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_publishes_zero_count() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let a = account_id(&env.state.db, "alice").await;
    let a_uid = user_id(&env.state.db, a).await;
    let (cancel, mut rx) = start_relay(&env).await;
    while rx.try_recv().is_ok() {} // drain probes

    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    run_pending_fanouts(&env).await;
    request(
        &env.app,
        "POST",
        "/api/v1/notifications/clear",
        Some(&alice),
        Some(json!({})),
    )
    .await;

    // Wait for the relayed count event (deadline-bounded).
    let want = format!("user:notification:{a_uid}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_count: Option<StreamEvent> = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) if ev.event == "notifications_count" && ev.stream == want => {
                last_count = Some(ev);
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(last_count.unwrap().payload["unread_count"], 0);
    cancel.cancel();
}

// ---------------------------------------------------------------------------
// Email jobs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn email_job_follows_user_settings() {
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
    // `email_notif_favourite` defaults false → no EmailSend.
    let id = post(&env.app, &alice, json!({"status": "x"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    let rest = run_pending_fanouts(&env).await;
    assert!(!rest.iter().any(|e| e.kind == JobKind::EmailSend));

    // Flip the flag → a favourite on a *new* status enqueues
    // EmailSend (a re-favourite of the same status is a dedupe hit —
    // no new row, no repeat email) with a non-secret payload.
    sqlx::query("UPDATE users SET email_notif_favourite = true WHERE account_id = $1")
        .bind(a)
        .execute(&env.state.db)
        .await
        .unwrap();
    let id2 = post(&env.app, &alice, json!({"status": "y"})).await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id2}/favourite"),
        Some(&bob),
        Some(json!({})),
    )
    .await;
    let rest = run_pending_fanouts(&env).await;
    let email = rest.iter().find(|e| e.kind == JobKind::EmailSend);
    let Some(email) = email else {
        panic!("no EmailSend job enqueued")
    };
    assert_eq!(email.payload["kind"], "favourite");
    assert_eq!(email.payload["account_id"], a);
    assert_eq!(email.payload["from_account_id"], b);
    let payload_text = email.payload.to_string();
    assert!(
        !payload_text.contains('@'),
        "payload leaks an address: {payload_text}"
    );
}

/// `status` events only reach recipients who could actually see the
/// post: a `direct` status never emits one (the `mention` event is the
/// DM surface — anything more would leak the conversation), and a
/// followers-only post skips subscribers who don't follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_notifications_respect_visibility() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let dave = register(&env.app, "dave", "d@x.com").await;
    let (a, b, c, d) = (
        account_id(&env.state.db, "alice").await,
        account_id(&env.state.db, "bob").await,
        account_id(&env.state.db, "carol").await,
        account_id(&env.state.db, "dave").await,
    );

    // Carol subscribes without following; dave follows with notify.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/subscribe"),
        Some(&carol),
        Some(json!({})),
    )
    .await;
    request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{a}/follow"),
        Some(&dave),
        Some(json!({"notify": true})),
    )
    .await;
    run_pending_fanouts(&env).await; // consume the follow event

    // A DM: only the mentioned account is notified — never subscribers
    // or notify-flagged followers, who aren't in the conversation.
    let dm = post(
        &env.app,
        &alice,
        json!({"status": "@bob secret", "visibility": "direct"}),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(
        rows_for(&env.state.db, b).await,
        vec![("mention".into(), a)]
    );
    assert!(rows_for(&env.state.db, c).await.is_empty());
    assert!(rows_for(&env.state.db, d).await.is_empty());

    // Followers-only: the notify follower sees it, the
    // notify-without-following subscriber can't — no event.
    post(
        &env.app,
        &alice,
        json!({"status": "for followers", "visibility": "private"}),
    )
    .await;
    run_pending_fanouts(&env).await;
    assert_eq!(rows_for(&env.state.db, d).await, vec![("status".into(), a)]);
    assert!(rows_for(&env.state.db, c).await.is_empty());

    // Public reaches both.
    let public_id = post(&env.app, &alice, json!({"status": "hello all"})).await;
    run_pending_fanouts(&env).await;
    assert_eq!(rows_for(&env.state.db, c).await, vec![("status".into(), a)]);
    assert_eq!(
        rows_for(&env.state.db, d).await,
        vec![("status".into(), a), ("status".into(), a)]
    );

    // Defense in depth: a stray `status` row pointing at a status the
    // recipient can't see (rows predating the gate, or a mention an
    // edit revoked) drops out of the read surface entirely.
    let dm_id: i64 = dm.parse().unwrap();
    notifications::insert(
        &env.state.db,
        c,
        a,
        notifications::NotificationType::Status,
        Some(dm_id),
        None,
    )
    .await
    .unwrap();
    let (_, _, list) = request(&env.app, "GET", "/api/v1/notifications", Some(&carol), None).await;
    let types: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["type"].as_str())
        .collect();
    assert_eq!(types, ["status"], "{list}");
    assert_eq!(
        list[0]["status"]["id"].as_str().unwrap(),
        public_id,
        "only the public post's notification survives"
    );
}
