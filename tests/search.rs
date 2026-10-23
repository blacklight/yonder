//! Epic-17 search integration tests — categorized results, filtering,
//! visibility, autocomplete, remote lookup gating and caching.
//! Runs against Postgres + Redis in testcontainers; skipped (with a
//! message, never silently green) when no container runtime is
//! reachable.

#![allow(clippy::result_large_err)]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::{postgres::Postgres, redis::Redis};
use tower::ServiceExt;
use yonder::{
    activitypub, cache,
    config::Config,
    db,
    http::{build_router, AppState},
};

const BASE: &str = "http://127.0.0.1:4000";
const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

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
    // High ceilings — the per-bucket tests need requests to pass.
    let rl = yonder::config::RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.search = rl.clone();
    config.rate_limit.authentication = rl.clone();
    config.rate_limit.account_creation = rl;
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

async fn register(app: &Router, username: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/accounts")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "username": username,
                        "email": format!("{username}@test.local"),
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
    (status, serde_json::from_slice(&bytes).unwrap_or(json!({})))
}

async fn post(app: &Router, token: &str, text: &str, visibility: &str) -> String {
    let (status, body) = request(
        app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": text, "visibility": visibility})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "post failed: {body}");
    body["id"].as_str().unwrap().to_string()
}

async fn account_id(db: &PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

/// `(Activity, raw JSON)` for a remote activity — the shape
/// `posts::remote::handle` consumes (same helper `tests/interactions.rs`
/// uses for inbound materialization).
fn activity(ty: &str, actor: &str, object: Value) -> (pubby_sdk::Activity, Value) {
    let a = pubby_sdk::Activity {
        id: format!("{}/activities/{}", actor, uuid::Uuid::new_v4()),
        activity_type: ty.into(),
        actor: actor.into(),
        object: object.clone(),
        to: vec![PUBLIC.into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let raw = json!({
        "@context": PUBLIC,
        "id": a.id,
        "type": ty,
        "actor": actor,
        "to": [PUBLIC],
        "object": object,
    });
    (a, raw)
}

/// Loopback mock AP server — the relaxed federation client
/// (`allow_private_targets`) reaches it over plain HTTP, so the
/// signed-GET fetch path is exercised without external network.
/// Returns the URL prefix (`http://localhost:{port}`) the search must
/// fetch. Routes: `/objects/m1` (trusted Note), `/objects/spoof` (id
/// on another origin), `/users/mallory` (actor doc), anything else 404.
async fn mock_remote() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://localhost:{port}");
    let note_base = base.clone();
    let actor_base = base.clone();
    let app = axum::Router::new()
        .route(
            "/objects/m1",
            axum::routing::get(move || {
                let b = note_base.clone();
                async move {
                    axum::Json(json!({
                        "@context": PUBLIC,
                        "id": format!("{b}/objects/m1"),
                        "type": "Note",
                        "attributedTo": format!("{b}/users/eve"),
                        "content": "<p>moonglow fetch marker</p>",
                        "to": [PUBLIC],
                        "published": "2025-01-01T00:00:00Z",
                    }))
                }
            }),
        )
        .route(
            "/objects/spoof",
            axum::routing::get(|| async {
                axum::Json(json!({
                    "@context": PUBLIC,
                    "id": "https://victim.example/notes/9",
                    "type": "Note",
                    "attributedTo": "https://victim.example/users/eve",
                    "content": "<p>stolen post</p>",
                    "to": [PUBLIC],
                }))
            }),
        )
        .route(
            "/users/mallory",
            axum::routing::get(move || {
                let b = actor_base.clone();
                async move {
                    axum::Json(json!({
                        "@context": PUBLIC,
                        "id": format!("{b}/users/mallory"),
                        "type": "Person",
                        "preferredUsername": "mallory",
                        "inbox": format!("{b}/users/mallory/inbox"),
                    }))
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn names(body: &Value, key: &str) -> Vec<String> {
    body[key]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    e["username"]
                        .as_str()
                        .or_else(|| e["name"].as_str())
                        .or_else(|| e.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Categorized results, pagination, shapes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn categorized_results_and_type_filter() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    post(&env.app, &alice, "hello world #cats", "public").await;
    let bob = register(&env.app, "bob").await;
    post(&env.app, &bob, "hello again from bob", "public").await;

    // Text search populates the statuses bucket ("hello" isn't a
    // username substring, so accounts stay empty here).
    let (status, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=hello",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["statuses"].as_array().unwrap().len(), 2);
    assert_eq!(body["hashtags"].as_array().unwrap().len(), 0);
    // `type` restricts to one bucket.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=alice&type=accounts",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(names(&body, "accounts"), vec!["alice".to_string()]);
    assert_eq!(body["statuses"].as_array().unwrap().len(), 0);
    assert_eq!(body["hashtags"].as_array().unwrap().len(), 0);

    // Hashtag match lands in the hashtags bucket (v2 Tag shape).
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%23cats",
        Some(&alice),
        None,
    )
    .await;
    let tags = body["hashtags"].as_array().unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0]["name"], "cats");
    assert_eq!(tags[0]["history"].as_array().unwrap().len(), 0);
    // v1 hashtag shape is the bare string.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v1/search?q=%23cats",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["hashtags"][0], json!("cats"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pagination_offset_and_limit() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    for i in 0..3 {
        post(&env.app, &alice, &format!("paged content {i}"), "public").await;
    }
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=paged&type=statuses&limit=1",
        Some(&alice),
        None,
    )
    .await;
    let first = body["statuses"][0]["id"].as_str().unwrap().to_string();
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=paged&type=statuses&limit=1&offset=1",
        Some(&alice),
        None,
    )
    .await;
    let second = body["statuses"][0]["id"].as_str().unwrap().to_string();
    assert_ne!(first, second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_search_scoped_by_account_id() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let bob = register(&env.app, "bob").await;
    post(&env.app, &alice, "scoped alice post", "public").await;
    post(&env.app, &bob, "scoped bob post", "public").await;
    let alice_id = account_id(&env.state.db, "alice").await;

    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q=scoped&type=statuses&account_id={alice_id}"),
        Some(&alice),
        None,
    )
    .await;
    let statuses = body["statuses"].as_array().unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0]["account"]["username"], "alice");

    let (status, _) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=scoped&account_id=99999999",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Visibility and filtering
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visibility_rules_for_anonymous_and_followers() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    post(&env.app, &alice, "vis public post", "public").await;
    post(&env.app, &alice, "vis followers post", "followers_only").await;

    // Anonymous sees public only.
    let (status, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=vis&type=statuses",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let found = body["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("public"));

    // A non-follower sees the same as anonymous.
    let bob = register(&env.app, "bob").await;
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=vis&type=statuses",
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1);

    // Follow → followers_only appears.
    let alice_id = account_id(&env.state.db, "alice").await;
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        None,
    )
    .await;
    assert!(status.is_success());
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=vis&type=statuses",
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 2);

    // The author always sees their own posts regardless of visibility.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=vis&type=statuses",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn author_opt_out_hides_posts_from_others() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    post(&env.app, &alice, "optout marker post", "public").await;
    sqlx::query("UPDATE users SET include_public_posts_in_search = false")
        .execute(&env.state.db)
        .await
        .unwrap();

    let bob = register(&env.app, "bob").await;
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=optout&type=statuses",
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 0);

    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=optout&type=statuses",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_suspended_and_undiscoverable_are_excluded() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let carol = register(&env.app, "carol").await;
    post(&env.app, &carol, "evil marker post", "public").await;
    let carol_id = account_id(&env.state.db, "carol").await;

    // Block hides carol's account and statuses from alice.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{carol_id}/block"),
        Some(&alice),
        None,
    )
    .await;
    assert!(status.is_success());
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=carol&type=accounts",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=evil&type=statuses",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 0);
    // …but not from other viewers.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=evil&type=statuses",
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1);

    // Suspension hides carol from everyone.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(carol_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=carol&type=accounts",
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discoverability_opt_outs_and_exceptions() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let _bob = register(&env.app, "bob").await;
    let bob_id = account_id(&env.state.db, "bob").await;

    // discoverable=false hides bob from text search.
    sqlx::query("UPDATE accounts SET discoverable = false WHERE id = $1")
        .bind(bob_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=bob&type=accounts",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);

    // …but a follower still finds them.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{bob_id}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert!(status.is_success());
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=bob&type=accounts",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 1);

    // A non-discoverable remote account is still returned on exact
    // FQN match.
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox, discoverable) \
         VALUES ('remote.example', 'hidden', $1, $1, '', false)",
    )
    .bind("https://remote.example/ap/users/hidden")
    .execute(&env.state.db)
    .await
    .unwrap();
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%40hidden%40remote.example&type=accounts",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(names(&body, "accounts"), vec!["hidden".to_string()]);

    // indexable=false hides a local account from text search too.
    sqlx::query("UPDATE users SET indexable = false WHERE account_id = $1")
        .bind(bob_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let dave = register(&env.app, "dave").await;
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=bob&type=accounts",
        Some(&dave),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn following_param_restricts_accounts() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let _carla = register(&env.app, "carla").await;
    let _carol = register(&env.app, "carol").await;
    let carol_id = account_id(&env.state.db, "carol").await;
    // alice follows carol but not carla — "car" prefixes both, the
    // following filter must drop carla.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{carol_id}/follow"),
        Some(&alice),
        None,
    )
    .await;
    assert!(status.is_success());

    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=car&type=accounts&following=true",
        Some(&alice),
        None,
    )
    .await;
    let found = names(&body, "accounts");
    assert_eq!(found, vec!["carol".to_string()], "{found:?}");
}

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autocomplete_prefix_matching() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let _albert = register(&env.app, "albert").await;
    let _bob = register(&env.app, "bob").await;
    post(&env.app, &alice, "one #cats and also #catnip", "public").await;

    // Prefix: "al" matches alice + albert, not bob.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=al&type=accounts&limit=5",
        Some(&alice),
        None,
    )
    .await;
    let mut found = names(&body, "accounts");
    found.sort();
    assert_eq!(found, vec!["albert".to_string(), "alice".to_string()]);

    // Prefix (not substring): "be" matches neither albert nor alice.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=be&type=accounts&limit=5",
        Some(&alice),
        None,
    )
    .await;
    assert!(!names(&body, "accounts").contains(&"albert".to_string()));

    // Hashtag prefix for autocomplete.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=cat&type=hashtags&limit=5",
        Some(&alice),
        None,
    )
    .await;
    let mut tags = names(&body, "hashtags");
    tags.sort();
    assert_eq!(tags, vec!["catnip".to_string(), "cats".to_string()]);
}

// ---------------------------------------------------------------------------
// Auth gates
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_search_gate() {
    // Enabled (default): anonymous search works.
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    post(&env.app, &alice, "anon searchable post", "public").await;
    let (status, _) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=anon&type=statuses",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Disabled: anonymous is rejected, authenticated still works.
    let mut config = Config::default();
    config.search.unauthenticated_enabled = false;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let (status, _) = request(&env.app, "GET", "/api/v2/search?q=anon", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(&env.app, "GET", "/api/v2/search?q=anon", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);

    // Missing q is a validation error (422 in this codebase).
    let (status, _) = request(&env.app, "GET", "/api/v2/search", Some(&alice), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// FQN / URL lookup
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_lookup_finds_local_status_and_account() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let id = post(&env.app, &alice, "url lookup target", "public").await;

    // Local status URL `{base}/@{user}/{id}`.
    let url = format!("{BASE}/@alice/{id}");
    let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}"),
        Some(&alice),
        None,
    )
    .await;
    let statuses = body["statuses"].as_array().unwrap();
    assert_eq!(statuses.len(), 1, "{body}");
    assert_eq!(statuses[0]["id"], id);

    // Local actor URL `{base}/@{user}` and the AP path.
    for path in [format!("{BASE}/@alice"), format!("{BASE}/ap/users/alice")] {
        let encoded = url::form_urlencoded::byte_serialize(path.as_bytes()).collect::<String>();
        let (_, body) = request(
            &env.app,
            "GET",
            &format!("/api/v2/search?q={encoded}"),
            Some(&alice),
            None,
        )
        .await;
        assert_eq!(
            body["accounts"][0]["username"], "alice",
            "lookup failed for {path}: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fqn_lookup_miss_is_negatively_cached() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;

    // Without resolve, nothing is fetched and no row is created.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%40ghost%40remote.example",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);

    // remote.example doesn't resolve: pubby's fabricated
    // `https://{d}/@{u}` answer is a miss, not an actor — no stub row,
    // and the failure is negatively cached for the TTL window.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%40ghost%40remote.example&resolve=true",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0, "{body}");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE username = 'ghost' AND domain = 'remote.example')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!exists, "unresolvable FQN must not leave a stub account");
    let cached = env
        .state
        .cache
        .get("search:neglookup:acct:ghost@remote.example")
        .await
        .unwrap();
    assert!(
        cached.is_some(),
        "failed FQN lookup was not negatively cached"
    );

    // A repeat within the TTL is answered by the negative cache.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%40ghost%40remote.example&resolve=true",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_fetch_disabled_gates_lookup() {
    let mut config = Config::default();
    config.search.remote_fetch = false;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=%40ghost%40remote.example&resolve=true",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE username = 'ghost' AND domain = 'remote.example')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!exists);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_url_fetch_is_negatively_cached() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let base = mock_remote().await;
    let url = format!("{base}/objects/missing");
    let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}&resolve=true"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0);
    assert_eq!(body["statuses"].as_array().unwrap().len(), 0);
    let cached = env
        .state
        .cache
        .get(&format!("search:neglookup:url:{url}"))
        .await
        .unwrap();
    assert!(cached.is_some(), "failed lookup was not negatively cached");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_lookup_respects_blocks_and_offset() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let _carol = register(&env.app, "carol").await;
    let _dave = register(&env.app, "dave").await;
    let carol_id = account_id(&env.state.db, "carol").await;
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{carol_id}/block"),
        Some(&alice),
        None,
    )
    .await;
    assert!(status.is_success());

    // A blocked actor's URL returns nothing — the social-graph
    // exclusions apply to exact lookups too.
    let encoded = url::form_urlencoded::byte_serialize(format!("{BASE}/@carol").as_bytes())
        .collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0, "{body}");

    // A non-blocked actor's URL still resolves.
    let encoded = url::form_urlencoded::byte_serialize(format!("{BASE}/@dave").as_bytes())
        .collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"][0]["username"], "dave", "{body}");

    // Lookups are single-hit: an offset page returns empty.
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}&offset=1"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"].as_array().unwrap().len(), 0, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_status_text_search_and_url_lookup() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";

    // A remote Note with HTML content and no `source` — most servers
    // send none. Materialization must derive searchable plain text.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/m1",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>zebrafrost remote <strong>marker</strong></p>",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    let stored: String = sqlx::query_scalar("SELECT content FROM statuses WHERE uri = $1")
        .bind("https://remote.test/objects/m1")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(stored, "zebrafrost remote marker");

    // Text search finds the remote post.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=zebrafrost&type=statuses",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1, "{body}");

    // Its remote URL resolves through the cached row — no fetch needed.
    let encoded = url::form_urlencoded::byte_serialize("https://remote.test/objects/m1".as_bytes())
        .collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_fetch_materializes_trusted_objects() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice").await;
    let base = mock_remote().await;

    // A fetched Note on its own origin materializes — URL lookup and
    // text search both find it.
    let url = format!("{base}/objects/m1");
    let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}&resolve=true"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1, "{body}");
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v2/search?q=moonglow&type=statuses",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["statuses"].as_array().unwrap().len(), 1, "{body}");

    // An actor document whose `id` is the fetched URL becomes an account.
    let url = format!("{base}/users/mallory");
    let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}&resolve=true"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(body["accounts"][0]["username"], "mallory", "{body}");

    // A document claiming another host's `id` is rejected — no
    // materialization, and the miss is negatively cached.
    let url = format!("{base}/objects/spoof");
    let encoded = url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>();
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v2/search?q={encoded}&resolve=true"),
        Some(&alice),
        None,
    )
    .await;
    assert!(body["statuses"].as_array().unwrap().is_empty(), "{body}");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE uri = 'https://victim.example/notes/9')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!exists, "spoofed remote object must not materialize");
    let cached = env
        .state
        .cache
        .get(&format!("search:neglookup:url:{url}"))
        .await
        .unwrap();
    assert!(cached.is_some(), "spoofed fetch was not negatively cached");
}
