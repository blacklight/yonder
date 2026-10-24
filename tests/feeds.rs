//! Epic-19 feed tests: `/users/{u}.atom|.rss`, `/tags/{t}.*` and
//! `/lists/{id}.*` — XML structure and escaping, public-only visibility,
//! `content.max_feed_items`, and list-feed auth (Bearer +
//! `?access_token=`).
//!
//! Same harness conventions as `tests/timelines.rs`; skipped when no
//! container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{header::HeaderMap, Request, StatusCode},
    Router,
};
use quick_xml::{events::Event, Reader};
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
};

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
    let pool = sqlx::PgPool::connect(&format!(
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
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-feeds-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();
    config.federation.enabled = true;
    config.federation.allow_private_targets = true;
    config.federation.base_url = Some("http://127.0.0.1:4000".to_string());

    let dispatcher = yonder::jobs::Dispatcher::new(&redis_handle, &config.jobs);
    let mut state = AppState::new(config, pool, redis_handle);
    state.ap = Some(
        activitypub::build(
            state.config().as_ref(),
            &state.db,
            &state.redis,
            &dispatcher,
            &state.bus,
        )
        .await
        .unwrap(),
    );
    let state = Arc::new(state);
    let app = build_router(state.clone());
    Some(Env {
        app,
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
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

/// `GET` returning the raw body — feeds are XML, not JSON.
async fn get(app: &Router, path: &str, token: Option<&str>) -> (StatusCode, HeaderMap, String) {
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
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

async fn post(app: &Router, token: &str, body: Value) -> String {
    let mut payload = json!({"status": "hello"});
    if let Value::Object(m) = body {
        for (k, v) in m {
            payload[k] = v;
        }
    }
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/statuses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "post failed: {}",
        String::from_utf8_lossy(&body)
    );
    let entity: Value = serde_json::from_slice(&body).unwrap();
    entity["id"].as_str().unwrap().to_string()
}

async fn reblog(app: &Router, token: &str, status_id: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/statuses/{status_id}/reblog"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn account_id(db: &PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Parse the whole document — feeds must be well-formed XML, not just
/// plausible-looking strings.
fn assert_well_formed(xml: &str) {
    let mut reader = Reader::from_str(xml);
    while !matches!(
        reader.read_event().expect("feed must be well-formed XML"),
        Event::Eof
    ) {}
}

// ---------------------------------------------------------------------------
// Profile feeds
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_feed_atom_and_rss() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    post(
        &env.app,
        &alice,
        json!({"status": "public <b>& \"quotes\"</b>", "visibility": "public"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "unlisted post", "visibility": "unlisted"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "followers post", "visibility": "private"}),
    )
    .await;

    // Atom.
    let (code, headers, body) = get(&env.app, "/users/alice.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-type"], "application/atom+xml; charset=utf-8",
        "{:?}",
        headers
    );
    assert_well_formed(&body);
    assert!(
        body.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"),
        "{body}"
    );
    assert!(body.contains("<id>"), "{body}");
    assert!(body.contains("rel=\"self\""), "{body}");
    assert!(body.contains("<published>"), "{body}");
    assert!(body.contains("<author><name>"), "{body}");
    // Content is escaped — the literal markup must not survive.
    assert!(body.contains("&lt;b&gt;"), "{body}");
    assert!(body.contains("&quot;"), "{body}");
    // Only the public post appears.
    assert_eq!(body.matches("<entry>").count(), 1, "{body}");
    assert!(!body.contains("unlisted post"), "{body}");
    assert!(!body.contains("followers post"), "{body}");

    // RSS.
    let (code, headers, body) = get(&env.app, "/users/alice.rss", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-type"], "application/rss+xml; charset=utf-8",
        "{:?}",
        headers
    );
    assert_well_formed(&body);
    assert!(body.contains("<rss version=\"2.0\""), "{body}");
    assert!(body.contains("<channel>"), "{body}");
    assert!(body.contains("<pubDate>"), "{body}");
    assert!(body.contains("<guid isPermaLink=\"false\">"), "{body}");
    assert!(body.contains("<dc:creator>"), "{body}");
    assert_eq!(body.matches("<item>").count(), 1, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_feed_lookup_and_404s() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env.app, "alice", "a@x.com").await;

    // Case-insensitive username.
    let (code, _, body) = get(&env.app, "/users/ALICE.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");

    for path in [
        "/users/nobody.atom",         // unknown username
        "/users/alice",               // no extension
        "/users/alice.json",          // unknown extension
        "/users/alice.atom.xml",      // double extension
        "/users/alice/statuses.atom", // multi-segment remainder
    ] {
        let (code, _, body) = get(&env.app, path, None).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{path} → {body}");
    }

    // Suspended accounts 404.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE username = 'alice'")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (code, _, body) = get(&env.app, "/users/alice.atom", None).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_feed_max_items() {
    let mut config = Config::default();
    config.content.max_feed_items = 2;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    for i in 0..4 {
        post(
            &env.app,
            &alice,
            json!({"status": format!("post {i}"), "visibility": "public"}),
        )
        .await;
    }
    let (code, _, body) = get(&env.app, "/users/alice.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_well_formed(&body);
    assert_eq!(body.matches("<entry>").count(), 2, "{body}");
    // Newest two win.
    assert!(body.contains("post 3"), "{body}");
    assert!(!body.contains("post 0"), "{body}");
    assert!(!body.contains("post 1"), "{body}");
}

// ---------------------------------------------------------------------------
// Hashtag feeds
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hashtag_feed_public_only_and_empty() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    post(
        &env.app,
        &alice,
        json!({"status": "loud #rust", "visibility": "public"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "quiet #rust", "visibility": "unlisted"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "untagged", "visibility": "public"}),
    )
    .await;

    let (code, headers, body) = get(&env.app, "/tags/rust.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-type"], "application/atom+xml; charset=utf-8",
        "{:?}",
        headers
    );
    assert_well_formed(&body);
    assert_eq!(body.matches("<entry>").count(), 1, "{body}");
    assert!(body.contains("loud"), "{body}");
    assert!(!body.contains("quiet"), "{body}");
    assert!(!body.contains("untagged"), "{body}");

    // Case-insensitive tag lookup; the feed is titled with the canonical tag.
    let (code, _, body) = get(&env.app, "/tags/RUST.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body.matches("<entry>").count(), 1, "{body}");
    assert!(body.contains("<title>#rust</title>"), "{body}");

    // Unknown tag → valid empty feed.
    let (code, _, body) = get(&env.app, "/tags/nonexistent.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_well_formed(&body);
    assert!(!body.contains("<entry>"), "{body}");

    // Percent-encoded tag path segment decodes and re-encodes in self link.
    let (code, _, body) = get(&env.app, "/tags/a%20b.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains("/tags/a%20b.atom"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_feed_keeps_replies_and_drops_direct() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let root = post(
        &env.app,
        &alice,
        json!({"status": "feed root", "visibility": "public"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "feed reply", "visibility": "public", "in_reply_to_id": root}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "feed secret", "visibility": "direct"}),
    )
    .await;

    let (code, _, body) = get(&env.app, "/users/alice.atom", None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_well_formed(&body);
    assert_eq!(body.matches("<entry>").count(), 2, "{body}");
    assert!(body.contains("feed reply"), "{body}");
    assert!(!body.contains("feed secret"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_bucket_rate_limits() {
    let mut config = Config::default();
    config.rate_limit.feed = RateLimit {
        limit: 2,
        period_seconds: 60,
        authenticated_limit: None,
    };
    let Some(env) = test_env(config).await else {
        return;
    };
    register(&env.app, "alice", "a@x.com").await;

    for _ in 0..2 {
        let (code, _, body) = get(&env.app, "/users/alice.atom", None).await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    let (code, headers, _) = get(&env.app, "/users/alice.atom", None).await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"), "{headers:?}");
}

// ---------------------------------------------------------------------------
// List feeds
// ---------------------------------------------------------------------------

async fn create_list(app: &Router, token: &str, title: &str) -> i64 {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/lists")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!({"title": title}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "list create: {}",
        String::from_utf8_lossy(&body)
    );
    let list: Value = serde_json::from_slice(&body).unwrap();
    list["id"].as_str().unwrap().parse().unwrap()
}

async fn add_list_member(app: &Router, token: &str, list_id: i64, account: i64) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/lists/{list_id}/accounts"))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!({"account_ids": [account]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_feed_auth_and_content() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let bob_id = account_id(&env.state.db, "bob").await;

    // Bob posts across the visibility spectrum.
    post(
        &env.app,
        &bob,
        json!({"status": "bob public", "visibility": "public"}),
    )
    .await;
    post(
        &env.app,
        &bob,
        json!({"status": "bob followers", "visibility": "private"}),
    )
    .await;
    post(
        &env.app,
        &bob,
        json!({"status": "bob direct", "visibility": "direct"}),
    )
    .await;

    // Alice's list contains bob; carol's identical setup is a foreign list.
    let list_id = create_list(&env.app, &alice, "friends").await;
    add_list_member(&env.app, &alice, list_id, bob_id).await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let carol_list = create_list(&env.app, &carol, "mine").await;
    add_list_member(&env.app, &carol, carol_list, bob_id).await;

    let path = format!("/lists/{list_id}.atom");

    // Anonymous → 401.
    let (code, _, _) = get(&env.app, &path, None).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    // Owner via Bearer → 200, list-scope content.
    let (code, headers, body) = get(&env.app, &path, Some(&alice)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-type"], "application/atom+xml; charset=utf-8",
        "{:?}",
        headers
    );
    assert_well_formed(&body);
    assert!(body.contains("friends"), "{body}");
    assert!(body.contains("bob public"), "{body}");
    // followers_only/private member posts appear for the owner…
    assert!(body.contains("bob followers"), "{body}");
    // …but `direct` never does.
    assert!(!body.contains("bob direct"), "{body}");

    // Owner via ?access_token= → same feed.
    let (code, _, body) = get(&env.app, &format!("{path}?access_token={alice}"), None).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains("bob public"), "{body}");

    // A member's boost adds no entry: boost rows carry no content of their own.
    let (_, _, before) = get(&env.app, &path, Some(&alice)).await;
    let entries_before = before.matches("<entry>").count();
    let alice_post = post(
        &env.app,
        &alice,
        json!({"status": "alice original", "visibility": "public"}),
    )
    .await;
    reblog(&env.app, &bob, &alice_post).await;
    let (code, _, body) = get(&env.app, &path, Some(&alice)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body.matches("<entry>").count(), entries_before, "{body}");

    // A token scoped to `read:statuses` does not cover `read:lists` → 403.
    let alice_user: i64 = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id \
                            WHERE a.username = 'alice' AND a.domain IS NULL",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let scoped = "feed-scoped-token";
    sqlx::query(
        "INSERT INTO oauth_access_tokens (user_id, token_hash, scopes) VALUES ($1, $2, $3)",
    )
    .bind(alice_user)
    .bind(yonder::http::middleware::auth::token_hash(scoped))
    .bind("read:statuses")
    .execute(&env.state.db)
    .await
    .unwrap();
    let (code, _, _) = get(&env.app, &path, Some(scoped)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    // Bob's token on Alice's list → 404 (foreign is indistinguishable).
    let (code, _, _) = get(&env.app, &path, Some(&bob)).await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    // Nonexistent list and bad extensions → 404.
    let (code, _, _) = get(&env.app, "/lists/999999.atom", Some(&alice)).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _, _) = get(&env.app, "/lists/notanumber.atom", Some(&alice)).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _, _) = get(&env.app, &format!("/lists/{list_id}.json"), Some(&alice)).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}
