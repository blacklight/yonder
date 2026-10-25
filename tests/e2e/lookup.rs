//! Epic-25 `GET /api/v1/accounts/lookup` tests: handle resolution for
//! `/@acct` profile URLs — anonymous local/known-remote reads, the
//! own-domain normalization, the 401-vs-404 remote-miss contract, and
//! parity with `GET /api/v1/accounts/{id}`.
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
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
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
    config.rate_limit.search = config.rate_limit.authentication.clone();
    config.rate_limit.timeline = config.rate_limit.authentication.clone();
    config.rate_limit.relation = config.rate_limit.authentication.clone();
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-lookup-{}", uuid::Uuid::new_v4()))
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
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

/// Raw GET against the app — the `request` helper minus JSON posting.
async fn get(app: &Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(json!({})))
}

async fn lookup(app: &Router, acct: Option<&str>, token: Option<&str>) -> (StatusCode, Value) {
    let path = match acct {
        Some(a) => format!("/api/v1/accounts/lookup?acct={a}"),
        None => "/api/v1/accounts/lookup".to_string(),
    };
    get(app, &path, token).await
}

async fn account_id(db: &sqlx::PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_handles_resolve_anonymously() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env.app, "alice", "alice@x.com").await;
    let id = account_id(&env.state.db, "alice").await;
    let (_, by_id) = get(&env.app, &format!("/api/v1/accounts/{id}"), None).await;

    for acct in ["alice", "@alice", "ALICE"] {
        let (status, body) = lookup(&env.app, Some(acct), None).await;
        assert_eq!(status, StatusCode::OK, "{acct}: {body}");
        // The lookup serves exactly the `GET /accounts/{id}` payload.
        assert_eq!(body, by_id, "{acct}: lookup must match GET by id");
    }

    // `user@<own-domain>` normalizes to the local handle — the account
    // is found even though local rows carry `domain IS NULL`.
    let own = env.state.config().instance.domain.clone();
    for acct in [
        format!("alice@{own}"),
        format!("alice@{}", own.to_uppercase()),
    ] {
        let (status, body) = lookup(&env.app, Some(&acct), None).await;
        assert_eq!(status, StatusCode::OK, "{acct}: {body}");
        assert_eq!(body["acct"], "alice");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_missing_acct_422() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    for acct in [
        None,
        Some(""),
        Some("a@b"),
        Some("a@b.c"),
        Some("bad!name@example.com"),
        Some(&"a".repeat(300)),
    ] {
        let (status, body) = lookup(&env.app, acct, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{acct:?}: {body}");
    }
    // Unknown local handle is a clean 404.
    let (status, _) = lookup(&env.app, Some("ghost"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_remote_anonymous_401_unless_fetch_disabled() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    // remote_fetch on + federation up: sign-in is the missing gate.
    let (status, body) = lookup(&env.app, Some("ghost@remote.example"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Same handle with remote_fetch off → 404 (sign-in can't help).
    let mut config = Config::default();
    config.search.remote_fetch = false;
    let Some(env) = test_env(config).await else {
        return;
    };
    let (status, body) = lookup(&env.app, Some("ghost@remote.example"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_remote_authed_paths() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;

    // Suspended domain → 404 without any outbound fetch (the
    // `domain_suspended` check inside `lookup_fqn` runs first).
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('bad.example', 'suspend')")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, body) = lookup(&env.app, Some("user@bad.example"), Some(&alice)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A recorded negative-lookup miss short-circuits without a fetch.
    env.state
        .cache
        .set(
            "search:neglookup:acct:ghost@missed.example",
            &json!({"not_found": true}),
            std::time::Duration::from_secs(600),
        )
        .await
        .unwrap();
    let (status, body) = lookup(&env.app, Some("ghost@missed.example"), Some(&alice)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // remote_fetch off → 404 even authenticated.
    let mut config = Config::default();
    config.search.remote_fetch = false;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice2", "alice2@x.com").await;
    let (status, body) = lookup(&env.app, Some("ghost@remote.example"), Some(&alice)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_domain_known_account_matches_get_by_id() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    // A DB-known remote account on a domain later suspended: parity
    // with `GET /accounts/{id}` (pre-existing semantics — neither
    // consults `domain_blocks`).
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('gone.example', 'mia', 'https://gone.example/users/mia', \
                 'https://gone.example/users/mia', '')",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('gone.example', 'suspend')")
        .execute(&env.state.db)
        .await
        .unwrap();

    let (status, by_lookup) = lookup(&env.app, Some("mia@gone.example"), None).await;
    let mia_id: i64 = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE actor_id = 'https://gone.example/users/mia'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (_, by_id) = get(&env.app, &format!("/api/v1/accounts/{mia_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{by_lookup}");
    assert_eq!(by_lookup, by_id, "suspended-domain parity pin");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_suspended_and_muted_accounts() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    register(&env.app, "bob", "bob@x.com").await;
    register(&env.app, "carol", "carol@x.com").await;
    register(&env.app, "dave", "dave@x.com").await;

    // Deleted account → 404.
    sqlx::query("UPDATE accounts SET deleted_at = now() WHERE username = 'carol'")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _) = lookup(&env.app, Some("carol"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Suspended account → 404 through `load_public_account`.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE username = 'dave'")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _) = lookup(&env.app, Some("dave"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A muted account still resolves — the viewer needs the profile
    // to unmute it (no search-visibility filtering).
    let bob_id = account_id(&env.state.db, "bob").await;
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/accounts/{bob_id}/mute"))
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (status, body) = lookup(&env.app, Some("bob"), Some(&alice)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["acct"], "bob");
}
