//! Epic-11 account-management integration tests: invites, auto-follow,
//! moves, deletion and archive transfer — against testcontainers
//! Postgres+Redis, driving jobs through their handlers directly.
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
    config::{Config, RateLimit, RegistrationMode},
    db,
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    users::{auto_follow, deletion, moves, relationships, transfer},
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

    // Rate-limit buckets are tiny by default — lift them for the suite.
    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();

    // Federation on: moves/deletion publish through the AP context.
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    // Email is off in tests — registered users are verified at once.
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-am-{}", uuid::Uuid::new_v4()))
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
    Some(Env {
        app,
        state,
        _pg: pg,
        _redis: redis,
    })
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

/// Register through the real endpoint; returns the access token.
async fn register(env: &Env, username: &str, invite: Option<&str>) -> (StatusCode, String) {
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
                "reason": "let me in",
                "invite_code": invite,
            }),
        ),
    )
    .await;
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    (status, token)
}

async fn account_id(state: &AppState, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// Seed a remote account row plus its `actor_cache` entry — pubby's
/// outbound fan-out resolves addressed inboxes from the cache before
/// fetching over HTTP (which no test remote could answer).
async fn seed_remote(state: &AppState, actor_id: &str) -> i64 {
    sqlx::query(
        "INSERT INTO actor_cache (actor_id, actor_data) VALUES ($1, $2) \
         ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(actor_id)
    .bind(json!({
        "id": actor_id,
        "type": "Person",
        "inbox": format!("{actor_id}/inbox"),
    }))
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox, shared_inbox) \
         VALUES ($1, $2, $3, $3, $4, $4) RETURNING id",
    )
    .bind(activitypub::moderation::actor_domain(actor_id))
    .bind(actor_id.trim_end_matches('/').rsplit('/').next().unwrap())
    .bind(actor_id)
    .bind(format!("{actor_id}/inbox"))
    .fetch_one(&state.db)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invites_crud_check_and_registration_gate() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let (status, token) = register(&env, "owner", None).await;
    assert_eq!(status, StatusCode::OK);

    // Create → list → check → revoke → check again.
    let (status, invite) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/yonder/invites", json!({ "max_uses": 1 })),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{invite}");
    let code = invite["code"].as_str().unwrap().to_string();
    assert!(invite["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/invite/{code}")));

    let (status, list) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/invites", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, check) = request(
        &env.app,
        json_req(
            "GET",
            &format!("/api/v1/yonder/invites/check/{code}"),
            json!(null),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["valid"], true);

    let invite_id = invite["id"].as_str().unwrap().to_string();
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/yonder/invites/{invite_id}"),
                json!(null),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, check) = request(
        &env.app,
        json_req(
            "GET",
            &format!("/api/v1/yonder/invites/check/{code}"),
            json!(null),
        ),
    )
    .await;
    assert_eq!(check["valid"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_registration_needs_a_valid_invite() {
    let mut config = Config::default();
    config.auth.registration_mode = RegistrationMode::Closed;
    let Some(env) = test_env(config).await else {
        return;
    };

    // No code → 403; a bad code → 422.
    let (status, _) = register(&env, "nope", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = register(&env, "nope2", Some("bogus")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A staff account's invite (admins always may) lets a user in.
    let admin_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (username, actor_id) VALUES ('staff', $1) RETURNING id",
    )
    .bind(format!("{BASE}/ap/users/staff"))
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET role = 'admin' WHERE id = $1")
        .bind(admin_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (account_id, email, password_hash, approved_at) \
         VALUES ($1, 'staff@x.com', 'x', now())",
    )
    .bind(admin_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO invites (code, created_by_account_id, max_uses) \
         VALUES ('letmein', $1, 1)",
    )
    .bind(admin_id)
    .execute(&env.state.db)
    .await
    .unwrap();

    let (status, token) = register(&env, "invited", Some("letmein")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!token.is_empty());
    // The invite recorded on the user and the use counted.
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = 'letmein'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(uses, 1);
    let recorded: bool = sqlx::query_scalar(
        "SELECT invite_id IS NOT NULL FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE a.username = 'invited'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(recorded);
    // max_uses exhausted → 422 for the next sign-up.
    let (status, _) = register(&env, "second", Some("letmein")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Two sign-ups racing one invite use: exactly one wins — the
    // atomic consumption can't overshoot max_uses.
    sqlx::query(
        "INSERT INTO invites (code, created_by_account_id, max_uses) VALUES ('race', $1, 1)",
    )
    .bind(admin_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    let (r1, r2) = tokio::join!(
        register(&env, "racer1", Some("race")),
        register(&env, "racer2", Some("race")),
    );
    let winners = [r1.0, r2.0]
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    assert_eq!(winners, 1, "concurrent uses must not exceed max_uses");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_required_invite_auto_approves() {
    let mut config = Config::default();
    config.auth.registration_mode = RegistrationMode::ApprovalRequired;
    let Some(env) = test_env(config).await else {
        return;
    };
    let (status, owner) = register(&env, "owner2", None).await;
    // The owner needs approval — and can't log in yet, so seed the
    // invite directly (the endpoint would refuse the token anyway).
    assert_eq!(status, StatusCode::OK, "{owner:?}");
    let owner_id = account_id(&env.state, "owner2").await;
    sqlx::query("INSERT INTO invites (code, created_by_account_id) VALUES ('in2', $1)")
        .bind(owner_id)
        .execute(&env.state.db)
        .await
        .unwrap();

    let (status, _) = register(&env, "guest", Some("in2")).await;
    assert_eq!(status, StatusCode::OK);
    let approved: bool = sqlx::query_scalar(
        "SELECT approved_at IS NOT NULL FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE a.username = 'guest'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        approved,
        "an invite under approval-required approves at once"
    );
}

// ---------------------------------------------------------------------------
// Auto-follow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_follow_runs_after_activation() {
    let mut config = Config::default();
    config.admin.auto_follow_accounts = vec!["staffer".into()];
    config.admin.auto_followed_by = vec!["greeter".into()];
    let Some(env) = test_env(config).await else {
        return;
    };
    register(&env, "staffer", None).await;
    register(&env, "greeter", None).await;
    register(&env, "newbie", None).await;
    // The spawned auto-follow task is fire-and-forget — give it a beat.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let newbie = account_id(&env.state, "newbie").await;
    let staffer = account_id(&env.state, "staffer").await;
    let greeter = account_id(&env.state, "greeter").await;
    let forward: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(newbie)
    .bind(staffer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let reverse: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(greeter)
    .bind(newbie)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(forward, "newbie must auto-follow staffer");
    assert!(reverse, "greeter must auto-follow newbie");

    // Remote entries in auto_followed_by are skipped — we cannot make
    // a remote actor follow anyone.
    let remote = seed_remote(&env.state, "https://remote.example/users/r").await;
    let mut config2 = env.state.config().as_ref().clone();
    config2.admin.auto_followed_by = vec!["r@remote.example".into()];
    let ctx = auto_follow::AutoFollowCtx {
        db: env.state.db.clone(),
        config: Arc::new(config2),
        ap: env.state.ap.clone(),
    };
    let second = account_id(&env.state, "greeter").await;
    auto_follow::apply(&ctx, second).await.unwrap();
    let bogus: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(remote)
    .bind(second)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!bogus);
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_delete_schedules_revokes_and_tombstones() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let (status, token) = register(&env, "leaver", None).await;
    assert_eq!(status, StatusCode::OK);
    let (_status, _) = register(&env, "stayer", None).await;
    let leaver = account_id(&env.state, "leaver").await;
    let stayer = account_id(&env.state, "stayer").await;
    // Some content + a follower for the delete fan-out.
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(stayer)
        .bind(leaver)
        .execute(&env.state.db)
        .await
        .unwrap();
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri) VALUES ($1, 'bye', $2) RETURNING id",
    )
    .bind(leaver)
    .bind(format!("{BASE}/ap/users/leaver/statuses/1"))
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // The endpoint confirms the password, schedules and returns 200.
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/account/delete",
                json!({ "password": "correct-horse" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM account_deletions WHERE account_id = $1 AND state = 'pending')",
    )
    .bind(leaver)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(pending);
    // Deactivated at once: suspended and every credential revoked.
    let (suspended, tokens): (bool, i64) = sqlx::query_as(
        "SELECT a.suspended_at IS NOT NULL, \
             (SELECT count(*) FROM oauth_access_tokens t JOIN users u ON u.id = t.user_id \
              WHERE u.account_id = $1 AND t.revoked_at IS NULL) \
         FROM accounts a WHERE a.id = $1",
    )
    .bind(leaver)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(suspended);
    assert_eq!(tokens, 0);

    // The token itself is dead now — verify_credentials rejects it.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Run the purge — tombstone: actor row stays, everything goes.
    let ctx = job_ctx(&env.state);
    deletion::AccountDeletionJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "account_id": leaver }))
        .await
        .unwrap();
    let (username, deleted, display): (String, bool, Option<String>) = sqlx::query_as(
        "SELECT username, deleted_at IS NOT NULL, display_name FROM accounts WHERE id = $1",
    )
    .bind(leaver)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(username, "leaver", "the tombstone keeps the username");
    assert!(deleted);
    assert_eq!(display, None);
    let users_left: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE account_id = $1)")
            .bind(leaver)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!users_left, "credentials must be gone");
    let status_gone: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM statuses WHERE id = $1)")
            .bind(status_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!status_gone);
    // The Delete activity is kept (deliveries cascade-protected) and
    // addressed to Public + followers.
    let delete: Option<Value> = sqlx::query_scalar(
        "SELECT object FROM activities WHERE activity_id LIKE '%#delete' AND local",
    )
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    let delete = delete.expect("the Delete activity must be stored");
    assert_eq!(delete["type"], "Delete");
    // The name can never be re-registered.
    let (status, _) = register(&env, "leaver", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_deletion_cancels_via_admin_path() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "graced", None).await;
    let graced = account_id(&env.state, "graced").await;
    deletion::schedule_account_deletion(
        &env.state,
        graced,
        None,
        std::time::Duration::from_secs(86_400),
    )
    .await
    .unwrap();
    deletion::cancel_account_deletion(&env.state.db, graced)
        .await
        .unwrap();
    let (cancelled, suspended): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM account_deletions WHERE account_id = $1 AND state = 'cancelled'), \
                a.suspended_at IS NOT NULL FROM accounts a WHERE a.id = $1",
    )
    .bind(graced)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(cancelled);
    assert!(!suspended, "cancellation lifts the deactivation");

    // A pre-existing (moderator) suspension survives cancellation.
    register(&env, "already_suspended", None).await;
    let modded = account_id(&env.state, "already_suspended").await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(modded)
        .execute(&env.state.db)
        .await
        .unwrap();
    deletion::schedule_account_deletion(
        &env.state,
        modded,
        None,
        std::time::Duration::from_secs(86_400),
    )
    .await
    .unwrap();
    deletion::cancel_account_deletion(&env.state.db, modded)
        .await
        .unwrap();
    let still_suspended: bool =
        sqlx::query_scalar("SELECT suspended_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(modded)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(still_suspended, "cancel must not lift a prior suspension");

    // Rescheduling after a cancel resurrects the row; the cron sweep
    // (empty payload) picks up the due deletion and purges.
    deletion::schedule_account_deletion(
        &env.state,
        graced,
        None,
        std::time::Duration::from_secs(86_400),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE account_deletions SET scheduled_at = now() - interval '1 hour' \
         WHERE account_id = $1",
    )
    .bind(graced)
    .execute(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    deletion::AccountDeletionJob::new(env.state.ap.clone())
        .run(&ctx, &json!({}))
        .await
        .unwrap();
    let purged: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(graced)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(purged, "the sweep must purge due deletions");
}

// ---------------------------------------------------------------------------
// Moves
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_move_redirects_and_migrates_followers() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let (_, origin_token) = register(&env, "origin", None).await;
    register(&env, "target", None).await;
    register(&env, "fan", None).await;
    // A second follower who blocks the target must NOT migrate.
    register(&env, "blocker", None).await;
    let origin = account_id(&env.state, "origin").await;
    let target = account_id(&env.state, "target").await;
    let fan = account_id(&env.state, "fan").await;
    let blocker = account_id(&env.state, "blocker").await;
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(fan)
        .bind(origin)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(blocker)
        .bind(origin)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO blocks (account_id, target_account_id) VALUES ($1, $2)")
        .bind(blocker)
        .bind(target)
        .execute(&env.state.db)
        .await
        .unwrap();

    // The target must claim the origin in its aliases first.
    moves::add_alias(&env.state, target, "origin")
        .await
        .unwrap();

    // Wrong password → 403 path.
    assert!(moves::move_to(&env.state, origin, "target", "wrong")
        .await
        .is_err());
    moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap();
    let moved_to: Option<i64> =
        sqlx::query_scalar("SELECT moved_to_account_id FROM accounts WHERE id = $1")
            .bind(origin)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(moved_to, Some(target));

    // The Move activity went out addressed to followers.
    let move_payload: Option<Value> =
        sqlx::query_scalar("SELECT object FROM activities WHERE type = 'Move' AND local")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    let move_payload = move_payload.expect("a stored Move activity");
    assert_eq!(
        move_payload["target"].as_str().unwrap(),
        format!("{BASE}/ap/users/target")
    );

    // verify_credentials renders `moved` as the nested target Account
    // for the *moved* account.
    let (status, me) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!(null)),
            &origin_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["moved"]["acct"].as_str(),
        Some("target"),
        "the moved entity nests the target Account: {me}"
    );

    // Run the migration job — the fan now follows target, not origin.
    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({ "direction": "local", "origin_id": origin, "target_id": target }),
        )
        .await
        .unwrap();
    let now_target: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let old_origin: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(fan)
    .bind(origin)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(now_target && !old_origin, "the follower must migrate");

    // The blocker stayed on the origin.
    let blocker_migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(blocker)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!blocker_migrated, "a follower blocking the target stays");

    // Re-moving inside the 30-day cooldown is a 429.
    let err = moves::move_to(&env.state, origin, "target", "correct-horse")
        .await
        .unwrap_err();
    assert!(
        matches!(err, yonder::Error::RateLimited(_)),
        "cooldown must rate-limit, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_move_verifies_alias_then_migrates() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "newhome", None).await;
    register(&env, "watcher", None).await;
    let newhome = account_id(&env.state, "newhome").await;
    let watcher = account_id(&env.state, "watcher").await;
    let origin_uri = "https://remote.example/users/mover";
    let origin = seed_remote(&env.state, origin_uri).await;
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(watcher)
        .bind(origin)
        .execute(&env.state.db)
        .await
        .unwrap();
    // The local target claims the origin in alsoKnownAs.
    sqlx::query("INSERT INTO account_aliases (account_id, uri) VALUES ($1, $2)")
        .bind(newhome)
        .bind(origin_uri)
        .execute(&env.state.db)
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({
                "direction": "remote",
                "origin_actor": origin_uri,
                "target_actor": format!("{BASE}/ap/users/newhome"),
            }),
        )
        .await
        .unwrap();

    let moved: bool =
        sqlx::query_scalar("SELECT moved_to_account_id = $2 FROM accounts WHERE id = $1")
            .bind(origin)
            .bind(newhome)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(moved, "the origin must point at the verified target");
    let now_follows: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(watcher)
    .bind(newhome)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(now_follows, "the local follower must migrate to the target");
    // The unfollow published an Undo aimed at the origin's inbox.
    let undo_delivery: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deliveries d JOIN activities a ON a.id = d.activity_id \
         WHERE d.inbox_url = $1 AND a.type = 'Undo')",
    )
    .bind(format!("{origin_uri}/inbox"))
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        undo_delivery,
        "the origin server must be told of the unfollow"
    );

    // Without an alias the same Move is dropped — origin unmarked.
    let origin2_uri = "https://remote.example/users/liar";
    let origin2 = seed_remote(&env.state, origin2_uri).await;
    moves::MigrateFollowersJob::new(env.state.ap.clone().unwrap())
        .run(
            &ctx,
            &json!({
                "direction": "remote",
                "origin_actor": origin2_uri,
                "target_actor": format!("{BASE}/ap/users/newhome"),
            }),
        )
        .await
        .unwrap();
    let moved2: Option<i64> =
        sqlx::query_scalar("SELECT moved_to_account_id FROM accounts WHERE id = $1")
            .bind(origin2)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(moved2, None);
}

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_export_and_archive_round_trip() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let (_, token) = register(&env, "exporter", None).await;
    register(&env, "friend", None).await;
    let exporter = account_id(&env.state, "exporter").await;
    let friend = account_id(&env.state, "friend").await;
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(exporter)
        .bind(friend)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO blocks (account_id, target_account_id) VALUES ($1, $2)")
        .bind(exporter)
        .bind(friend)
        .execute(&env.state.db)
        .await
        .unwrap();

    // CSV download inline.
    let res = env
        .app
        .clone()
        .oneshot(authed(
            json_req(
                "POST",
                "/api/v1/yonder/exports",
                json!({ "kind": "following" }),
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let csv = String::from_utf8(body.to_vec()).unwrap();
    assert!(csv.contains("Account address,Show boosts,Notify on new posts"));
    assert!(csv.contains("friend@localhost"));

    // The archive export queues (202) and the cooldown rate-limits a
    // second request inside 7 days (429 + Retry-After).
    let (status, export) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/exports",
                json!({ "kind": "archive" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{export}");
    let export_id: i64 = export["id"].as_str().unwrap().parse().unwrap();
    let res = env
        .app
        .clone()
        .oneshot(authed(
            json_req(
                "POST",
                "/api/v1/yonder/exports",
                json!({ "kind": "archive" }),
            ),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers().get("retry-after").is_some());

    // A failed export must not hold the cooldown: mark the row failed
    // and a fresh archive request is accepted again.
    sqlx::query("UPDATE archive_exports SET state = 'failed' WHERE id = $1")
        .bind(export_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, export2) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/exports",
                json!({ "kind": "archive" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{export2}");
    let export_id: i64 = export2["id"].as_str().unwrap().parse().unwrap();

    // Run the Export job — the zip must be downloadable.
    let ctx = job_ctx(&env.state);
    transfer::ExportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "export_id": export_id }))
        .await
        .unwrap();
    let res = env
        .app
        .clone()
        .oneshot(authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/yonder/exports/{export_id}/download"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let zip_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes.to_vec())).unwrap();
    let names: Vec<String> = zip.file_names().map(|n| n.to_string()).collect();
    for member in [
        "actor.json",
        "outbox.json",
        "following_accounts.csv",
        "blocked_accounts.csv",
        "bookmarks.json",
        "filters.json",
    ] {
        assert!(
            names.contains(&member.to_string()),
            "missing {member}: {names:?}"
        );
    }
    let mut outbox = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("outbox.json").unwrap(), &mut outbox).unwrap();
    assert!(outbox.contains("OrderedCollection"));

    // Expired export → 410.
    sqlx::query("UPDATE archive_exports SET expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(export_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let res = env
        .app
        .clone()
        .oneshot(authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/yonder/exports/{export_id}/download"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_applies_rows_and_posts_keep_source_ids() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer", None).await;
    register(&env, "oldfriend", None).await;
    let importer = account_id(&env.state, "importer").await;
    let friend = account_id(&env.state, "oldfriend").await;

    // A following CSV (with the Mastodon header) queued like the
    // endpoint would, then driven through the job.
    let csv = b"Account address,Show boosts,Notify on new posts,Languages\noldfriend@localhost,true,false,\n".to_vec();
    let key = format!("imports/{importer}/test.csv");
    env.state
        .media
        .put(&key, &csv, "application/octet-stream")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key, total) \
         VALUES ($1, 'following', 'merge', $2, 1) RETURNING id",
    )
    .bind(importer)
    .bind(&key)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (done, following): (bool, bool) = sqlx::query_as(
        "SELECT i.state = 'done', EXISTS(SELECT 1 FROM follows \
         WHERE account_id = $1 AND target_account_id = $2) \
         FROM archive_imports i WHERE i.id = $3",
    )
    .bind(importer)
    .bind(friend)
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(done);
    assert!(following);
    // The upload is gone once the import finishes.
    assert!(env.state.media.get(&key).await.is_err());

    // An archive with one outbox Create preserves the source id and
    // timestamp, mints a local URI and federates nothing.
    let note_id = "https://old.example/statuses/900";
    let outbox = json!({
        "type": "OrderedCollection",
        "orderedItems": [{
            "type": "Create",
            "actor": "https://old.example/users/importer",
            "object": {
                "id": note_id,
                "type": "Note",
                "attributedTo": "https://old.example/users/importer",
                "content": "remembered post",
                "published": "2020-01-02T03:04:05Z",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            },
        }],
    });
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut zip_buf);
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("outbox.json", opts).unwrap();
        std::io::Write::write_all(&mut w, outbox.to_string().as_bytes()).unwrap();
        w.finish().unwrap();
    }
    let zip_bytes = zip_buf.into_inner();
    let zkey = format!("imports/{importer}/archive.zip");
    env.state
        .media
        .put(&zkey, &zip_bytes, "application/zip")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key) \
         VALUES ($1, 'archive', 'merge', $2) RETURNING id",
    )
    .bind(importer)
    .bind(&zkey)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();

    let (uri, created, orig, local): (String, String, String, bool) = sqlx::query_as(
        "SELECT uri, to_char(created_at, 'YYYY-MM-DD HH24:MI:SS'), original_id, local \
         FROM statuses WHERE account_id = $1 AND original_id = $2",
    )
    .bind(importer)
    .bind(note_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(orig, note_id);
    assert!(local);
    assert!(
        uri.starts_with(&format!("{BASE}/ap/users/importer/statuses/")),
        "imported posts get local URIs, got {uri}"
    );
    assert_eq!(created, "2020-01-02 03:04:05", "source timestamp preserved");
    // No deliveries — imported content never goes out.
    let delivered: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deliveries d JOIN activities a ON a.id = d.activity_id)",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!delivered);

    // Re-importing the same archive is a no-op (dedupe on original_id).
    let zkey2 = format!("imports/{importer}/archive2.zip");
    env.state
        .media
        .put(&zkey2, &zip_bytes, "application/zip")
        .await
        .unwrap();
    let import_id2: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key) \
         VALUES ($1, 'archive', 'merge', $2) RETURNING id",
    )
    .bind(importer)
    .bind(&zkey2)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id2 }))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND original_id = $2",
    )
    .bind(importer)
    .bind(note_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(count, 1, "re-import must dedupe on original_id");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_follow_primitive_publishes_real_activities() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "me", None).await;
    let me = account_id(&env.state, "me").await;
    let remote = seed_remote(&env.state, "https://remote.example/users/them").await;

    let cfg = env.state.config();
    let rel = relationships::Ctx::for_parts(&env.state.db, &cfg, env.state.ap.as_ref());
    let outcome = relationships::follow(rel, me, remote, relationships::FollowOpts::default())
        .await
        .unwrap();
    assert_eq!(outcome, relationships::FollowOutcome::Requested);
    // Pending request locally, a real Follow on the wire.
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follow_requests WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(me)
    .bind(remote)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(pending);
    let follow_activity: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM activities WHERE type = 'Follow' AND local)",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        follow_activity,
        "a remote follow must publish a Follow activity"
    );
    let delivered: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM deliveries WHERE inbox_url = $1)")
            .bind("https://remote.example/users/them/inbox")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(delivered, "the Follow must be durably delivered");

    // Unfollow sends the Undo to the same inbox.
    let removed = relationships::unfollow(rel, me, remote).await.unwrap();
    assert!(removed);
    let undo: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM deliveries d JOIN activities a ON a.id = d.activity_id \
         WHERE d.inbox_url = $1 AND a.type = 'Undo')",
    )
    .bind("https://remote.example/users/them/inbox")
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(undo);
}

/// Request-time row counting: the header row is excluded and the 20k
/// Mastodon cap rejects oversize files before the job ever sees them.
#[test]
fn count_rows_sniffs_header_and_limit() {
    let csv =
        b"Account address,Show boosts,Notify on new posts,Languages\nme@x.example,true,false,\n";
    assert_eq!(
        transfer::count_rows(transfer::Kind::Following, csv).unwrap(),
        Some(1)
    );
    // Headerless counts every row.
    let bare = b"me@x.example\nyou@x.example\n";
    assert_eq!(
        transfer::count_rows(transfer::Kind::Blocking, bare).unwrap(),
        Some(2)
    );
    let mut big = String::from("Account address,Show boosts\n");
    for i in 0..20_001 {
        big.push_str(&format!("u{i}@x.example,true\n"));
    }
    assert!(transfer::count_rows(transfer::Kind::Following, big.as_bytes()).is_err());
    // Archive/filters have no request-time count.
    assert_eq!(
        transfer::count_rows(transfer::Kind::Archive, b"xx").unwrap(),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_overwrite_removes_absent_entries() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "overwriter", None).await;
    register(&env, "kept", None).await;
    register(&env, "dropped", None).await;
    let importer = account_id(&env.state, "overwriter").await;
    let kept = account_id(&env.state, "kept").await;
    let dropped = account_id(&env.state, "dropped").await;
    for t in [kept, dropped] {
        sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
            .bind(importer)
            .bind(t)
            .execute(&env.state.db)
            .await
            .unwrap();
    }

    // Headerless `following` file in overwrite mode: entries absent
    // from the file are unfollowed.
    let csv = b"kept@localhost,true,false,\n".to_vec();
    let key = format!("imports/{importer}/overwrite.csv");
    env.state
        .media
        .put(&key, &csv, "application/octet-stream")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key, total) \
         VALUES ($1, 'following', 'overwrite', $2, 1) RETURNING id",
    )
    .bind(importer)
    .bind(&key)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();

    let (kept_f, dropped_f): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2), \
                EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $3)",
    )
    .bind(importer)
    .bind(kept)
    .bind(dropped)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(kept_f, "the listed entry must survive");
    assert!(
        !dropped_f,
        "overwrite unfollows entries absent from the file"
    );
}
