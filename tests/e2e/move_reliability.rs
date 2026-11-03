//! Move-reliability baseline tests: durable migration events,
//! retryable publication, and failure-retaining follower migration.
//!
//! `move` notifications are still `#[ignore]`d until the
//! notifications workstream lands (contract section 4).
//!
//! Skipped when no container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use redis::AsyncCommands;
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{queue, JobContext, JobEnvelope, JobHandler, JobKind},
    notifications::fanout::NotificationFanoutJob,
    users::moves,
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
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-mr-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();
    config.media.backend = yonder::config::MediaBackend::Local;

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
    let app = build_router(state.clone());
    Some(Env { app, state })
}

fn job_ctx(state: &AppState) -> JobContext {
    JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    }
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

async fn register(env: &Env, username: &str) -> String {
    let (status, body) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": username,
                "email": format!("{username}@example.com"),
                "password": "correct-horse",
                "agreement": true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register {username}");
    body["access_token"].as_str().unwrap().to_string()
}

async fn account_id(state: &AppState, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

fn follow(
    db: &sqlx::PgPool,
    follower: i64,
    target: i64,
) -> impl std::future::Future<Output = ()> + '_ {
    let db = db.clone();
    async move {
        sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
            .bind(follower)
            .bind(target)
            .execute(&db)
            .await
            .unwrap();
    }
}

/// A move creates a durable `migration_events` row whose id is a
/// stable minted identifier — not derived from `count(*)` over the
/// activities table. The publication work keys the Move's activity
/// id to that event id (`{actor}#moves/{event_id}`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_creates_stable_migration_event() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let (event_id, state): (i64, String) = sqlx::query_as(
        "SELECT id, state FROM migration_events WHERE source_account_id = $1 \
         AND target_account_id = $2",
    )
    .bind(origin)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(event_id > 0, "a durable migration event must exist");
    assert_ne!(state, "failed");

    // The Move activity id is minted from the event, not a global
    // activity count.
    let work_activity: Option<String> = sqlx::query_scalar(
        "SELECT activity_id FROM publication_work WHERE activity_type = 'Move' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let work_activity = work_activity.expect("a pending Move work item must exist");
    assert!(
        work_activity.ends_with(&format!("#moves/{event_id}")),
        "activity id {work_activity} must be derived from event {event_id}"
    );
}

/// A failed follow inside the batch is retained for retry — the
/// cursor must not advance past it and lose the follower. A
/// *permanently* failed outcome must not spin the job: it is excluded
/// from future passes and the event ends 'failed'.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_follower_is_retained_for_retry() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    follow(&env.state.db, fan, origin).await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM migration_events WHERE source_account_id = $1 \
         AND target_account_id = $2",
    )
    .bind(origin)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // A failed outcome whose retry deadline passed must be re-attempted
    // and completed by the next batch.
    sqlx::query(
        "INSERT INTO migration_follower_outcomes \
         (event_id, follower_account_id, state, attempts, next_retry_at, error) \
         VALUES ($1, $2, 'failed', 1, now() - interval '1 minute', 'simulated')",
    )
    .bind(event_id)
    .bind(fan)
    .execute(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(migrated, "retry must complete the failed follow");

    // No failed outcome rows may linger after a successful retry.
    let failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM migration_follower_outcomes \
         WHERE state = 'failed' AND follower_account_id = $1",
    )
    .bind(fan)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(failed, 0, "failed outcomes must be retried to completion");
}

/// A permanently failed outcome (`next_retry_at NULL`) is excluded
/// from future passes — the event ends 'failed' instead of spinning
/// or pretending success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_failure_fails_the_event() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    follow(&env.state.db, fan, origin).await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();
    let event_id: i64 = sqlx::query_scalar(
        "SELECT id FROM migration_events WHERE source_account_id = $1 \
         AND target_account_id = $2",
    )
    .bind(origin)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Pre-seed a permanently failed outcome (attempt cap reached).
    sqlx::query(
        "INSERT INTO migration_follower_outcomes \
         (event_id, follower_account_id, state, attempts, next_retry_at, error) \
         VALUES ($1, $2, 'failed', 5, NULL, 'permanent')",
    )
    .bind(event_id)
    .bind(fan)
    .execute(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();

    let (state, attempts): (String, i32) = sqlx::query_as(
        "SELECT (SELECT state FROM migration_events WHERE id = $1), \
         (SELECT attempts FROM migration_follower_outcomes \
          WHERE event_id = $1 AND follower_account_id = $2)",
    )
    .bind(event_id)
    .bind(fan)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(state, "failed", "terminal follower failure fails the event");
    assert_eq!(attempts, 5, "a terminal outcome must not be retried");
}

/// An inbound Move replayed by the remote (same activity) coalesces
/// into one migration event and one follower migration — not two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_inbound_move_coalesces() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "target").await;
    register(&env, "fan").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    let target_actor: String = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(target)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let origin_uri = "https://remote.example/users/mover";
    let origin: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('remote.example', 'mover', $1, $1, $2) RETURNING id",
    )
    .bind(origin_uri)
    .bind(format!("{origin_uri}/inbox"))
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    follow(&env.state.db, fan, origin).await;
    sqlx::query("INSERT INTO account_aliases (account_id, uri) VALUES ($1, $2)")
        .bind(target)
        .bind(origin_uri)
        .execute(&env.state.db)
        .await
        .unwrap();

    // Drive the real inbound path twice with the same activity id:
    // the local target's alias verifies it, so each run verifies +
    // migrates (or coalesces onto the verified event).
    let ctx = job_ctx(&env.state);
    let payload = json!({
        "direction": "remote",
        "origin_actor": origin_uri,
        "target_actor": target_actor,
        "activity_id": "https://remote.example/activities/move/1",
    });
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(&ctx, &payload)
        .await
        .unwrap();
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(&ctx, &payload)
        .await
        .unwrap();

    // One migration event per verified move — replays do not stack.
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM migration_events \
         WHERE source_account_id = $1 AND target_account_id = $2",
    )
    .bind(origin)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(events, 1, "replays must coalesce, got {events} events");

    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(migrated, "the verified move must migrate the follower");
}

/// `GET /api/v1/yonder/move` reports state/cooldown so settings can
/// render progress without inferring it from a bare 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_state_endpoint_reports_progress() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "mover").await;

    let (mut parts, body) = json_req("GET", "/api/v1/yonder/move", json!(null)).into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (status, body) = request(&env.app, Request::from_parts(parts, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["moved"], false, "a fresh account has not moved");
    assert_eq!(body["event"], Value::Null, "no event before a move");

    // After a real move the endpoint must surface the event state
    // and follower-progress counters.
    register(&env, "destination").await;
    let mover = account_id(&env.state, "mover").await;
    let destination = account_id(&env.state, "destination").await;
    moves::add_alias(&env.state, destination, "mover")
        .await
        .unwrap();
    moves::move_to(&env.state, mover, "destination", "correct-horse")
        .await
        .unwrap();
    let (mut parts, body) = json_req("GET", "/api/v1/yonder/move", json!(null)).into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (status, body) = request(&env.app, Request::from_parts(parts, body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["moved"], true);
    assert!(
        body["event"]["state"].as_str().is_some(),
        "the event must report a state: {body}"
    );
    assert!(
        body["followers"].get("migrated").is_some(),
        "follower counters must be present: {body}"
    );
}

/// Drain pending `queue:default` envelopes and run the fanout jobs —
/// the test stands in for the worker, like `notifications` does.
async fn run_pending_fanouts(env: &Env) {
    let key = env.state.redis.key("queue:default");
    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn.xrange(&key, "-", "+").await.unwrap();
    let ctx = job_ctx(&env.state);
    for entry in reply.ids {
        if let Some(v) = entry.map.get("job") {
            let body: String = redis::from_redis_value(v).unwrap();
            let envelope: JobEnvelope = queue::decode(&body).unwrap();
            if envelope.kind == JobKind::NotificationFanout {
                NotificationFanoutJob
                    .run(&ctx, &envelope.payload)
                    .await
                    .unwrap();
            }
        }
        let _: i64 = conn.xdel(&key, &[entry.id]).await.unwrap();
    }
}

/// A verified move emits exactly one `move` notification to each
/// original local follower — Akkoma's extension type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_notifies_original_followers_once() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    follow(&env.state.db, fan, origin).await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();
    // Run again — retries and replays must not duplicate.
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();
    run_pending_fanouts(&env).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications \
         WHERE account_id = $1 AND type = 'move'",
    )
    .bind(fan)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(count, 1, "exactly one move notification per follower");
}

/// A locked *local* target receives a verified follow directly (not
/// a request), and an existing follow keeps its own preferences —
/// the origin's flags only seed a brand-new row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_target_gets_verified_follow_with_options() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    // The follower watches the origin with non-default flags; the
    // migration must carry them, not reset to defaults.
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify) \
         VALUES ($1, $2, false, true)",
    )
    .bind(fan)
    .bind(origin)
    .execute(&env.state.db)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(target)
        .execute(&env.state.db)
        .await
        .unwrap();
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();

    // Verified migration follow — no pending request remains.
    let (migrated, show_reblogs, notify): (bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM follows \
             WHERE account_id = $1 AND target_account_id = $2), \
         (SELECT show_reblogs FROM follows \
             WHERE account_id = $1 AND target_account_id = $2), \
         (SELECT notify FROM follows \
             WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(migrated, "locked local targets get a direct follow");
    assert!(!show_reblogs, "origin show_reblogs must carry over");
    assert!(notify, "origin notify must carry over");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM follow_requests \
         WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(pending, 0, "migration must not leave a follow request");
}

/// A follower who already follows the target keeps their own
/// preferences — migration must not overwrite them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_target_follow_keeps_its_preferences() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify) \
         VALUES ($1, $2, false, false)",
    )
    .bind(fan)
    .bind(origin)
    .execute(&env.state.db)
    .await
    .unwrap();
    // Already follows the target with different flags.
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify) \
         VALUES ($1, $2, true, true)",
    )
    .bind(fan)
    .bind(target)
    .execute(&env.state.db)
    .await
    .unwrap();
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();

    let (show_reblogs, notify): (bool, bool) = sqlx::query_as(
        "SELECT show_reblogs, notify FROM follows \
         WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(show_reblogs && notify, "existing preferences must survive");
    let still_following_origin: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(origin)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!still_following_origin, "the origin follow is released");
}

/// Observer state follows the moved account: the follower's private
/// note and list membership point at the target after migration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observer_state_carries_to_the_target() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    follow(&env.state.db, fan, origin).await;
    sqlx::query(
        "INSERT INTO account_notes (account_id, target_account_id, comment) \
         VALUES ($1, $2, 'met at a conference')",
    )
    .bind(fan)
    .bind(origin)
    .execute(&env.state.db)
    .await
    .unwrap();
    let list_id: i64 = sqlx::query_scalar(
        "INSERT INTO lists (account_id, title) VALUES ($1, 'friends') RETURNING id",
    )
    .bind(fan)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO list_accounts (list_id, account_id) VALUES ($1, $2)")
        .bind(list_id)
        .bind(origin)
        .execute(&env.state.db)
        .await
        .unwrap();
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();

    let note: Option<String> = sqlx::query_scalar(
        "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(fan)
    .bind(target)
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    assert_eq!(note.as_deref(), Some("met at a conference"));
    let in_list: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM list_accounts WHERE list_id = $1 AND account_id = $2)",
    )
    .bind(list_id)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(in_list, "list membership must carry to the target");
}

/// New follows to a redirected account are refused — followers must
/// be pointed at the move target instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_to_a_moved_account_are_rejected() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "origin").await;
    register(&env, "target").await;
    let fan_token = register(&env, "fan").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();

    let req = authed(
        json_req(
            "POST",
            &format!("/api/v1/accounts/{origin}/follow"),
            json!(null),
        ),
        &fan_token,
    );
    let (status, _) = request(&env.app, req).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "follows to a moved account must be refused"
    );
}

/// `POST /api/pleroma/move_account` behaves like the Yonder route —
/// password-confirmed, durable event, redirect set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_move_account_matches_yonder_move() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "origin").await;
    register(&env, "target").await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/pleroma/move_account",
                json!({ "password": "correct-horse", "target_account": "target" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pleroma move: {body}");
    let moved: bool =
        sqlx::query_scalar("SELECT moved_to_account_id = $2 FROM accounts WHERE id = $1")
            .bind(origin)
            .bind(target)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(moved, "the redirect must be set");
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM migration_events \
         WHERE source_account_id = $1 AND target_account_id = $2",
    )
    .bind(origin)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(events, 1, "one durable migration event");
}

/// `GET/PUT/DELETE /api/pleroma/aliases` round-trips the alias list
/// under `{"aliases": [...]}` like upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_aliases_round_trip() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "user").await;
    register(&env, "alsoknown").await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "PUT",
                "/api/pleroma/aliases",
                json!({ "alias": "alsoknown" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add alias: {body}");
    // Upstream mutations return `{"status": "success"}`; only GET lists.
    assert_eq!(body["status"], "success", "add alias shape: {body}");

    let (status, body) = request(
        &env.app,
        authed(json_req("GET", "/api/pleroma/aliases", json!(null)), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let aliases = body["aliases"].as_array().unwrap();
    assert!(
        !aliases.is_empty(),
        "alias list must include the new entry: {body}"
    );

    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                "/api/pleroma/aliases",
                json!({ "alias": "alsoknown" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `POST /api/pleroma/follow_import` queues the same import job as
/// the CSV upload, keyed to a stored `{list}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_follow_import_queues_an_import() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "importer").await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/pleroma/follow_import",
                json!({ "list": "alice@remote.example\nbob@remote.example\n" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "follow import: {body}");
    let kind = body["kind"].as_str().unwrap_or_default();
    assert_eq!(kind, "following", "import kind must be 'following'");
}
