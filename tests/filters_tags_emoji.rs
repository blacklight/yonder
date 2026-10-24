//! Epic-20 integration tests: `/api/v2/filters` CRUD + application
//! (home hide / `filtered` attr / notifications), tag lookup +
//! follow/unfollow + followed-tag home delivery + trends, and the
//! custom-emoji surface (upload, list, render, zip + dir import).
//!
//! Same harness conventions as `tests/feeds.rs`; skipped when no
//! container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
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

    for bucket in [
        &mut config.rate_limit.authentication,
        &mut config.rate_limit.timeline,
        &mut config.rate_limit.relation,
        &mut config.rate_limit.media,
    ] {
        *bucket = RateLimit {
            limit: 10_000,
            period_seconds: 60,
            authenticated_limit: None,
        };
    }
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-e20-{}", uuid::Uuid::new_v4()))
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
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

/// Promote an account to `admin` — the role joins at request time,
/// and the user's tokens gain the `admin` scope so
/// `require_scope(..., "admin")` passes its coverage check.
async fn make_admin(env: &Env, username: &str) {
    sqlx::query("UPDATE accounts SET role = 'admin' WHERE username = $1")
        .bind(username)
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

async fn post_status(app: &Router, token: &str, status: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/statuses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!({"status": status}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn req(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Minimal multipart body: `file` + text fields.
fn multipart(file_name: &str, file: &[u8], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "yondertestboundary";
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{file_name}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), out)
}

fn png() -> Vec<u8> {
    [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
    .to_vec()
}

// ---------- filters ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_crud_and_home_hide() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    let bob = register(&env.app, "bob", "b@x.test").await;

    // Bob follows Alice so her posts reach his home feed.
    let alice_id = sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE username = 'alice'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let (_, _) = req(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        None,
    )
    .await;

    let matching = post_status(&env.app, &alice, "spoilers for the finale inside").await;
    let clean = post_status(&env.app, &alice, "a perfectly normal post").await;

    // Home shows both before the filter exists.
    let (_, home) = req(&env.app, "GET", "/api/v1/timelines/home", Some(&bob), None).await;
    let ids: Vec<&str> = home
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&matching["id"].as_str().unwrap()));
    assert!(ids.contains(&clean["id"].as_str().unwrap()));

    // Create a hide filter on "spoilers" in the home context.
    let (status, filter) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&bob),
        Some(json!({
            "title": "finale",
            "context": ["home"],
            "filter_action": "hide",
            "keywords_attributes": [{"keyword": "spoilers", "whole_word": false}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filter}");
    assert_eq!(filter["filter_action"], "hide");
    assert_eq!(filter["context"], json!(["home"]));
    assert_eq!(filter["keywords"][0]["keyword"], "spoilers");

    // The matching post drops out; the clean post stays.
    let (_, home) = req(&env.app, "GET", "/api/v1/timelines/home", Some(&bob), None).await;
    let ids: Vec<&str> = home
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(!ids.contains(&matching["id"].as_str().unwrap()));
    assert!(ids.contains(&clean["id"].as_str().unwrap()));

    // Ownership: Alice cannot read or delete Bob's filter.
    let filter_id = filter["id"].as_str().unwrap();
    let (status, _) = req(
        &env.app,
        "GET",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = req(
        &env.app,
        "DELETE",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Keyword sub-resource roundtrip.
    let (status, kw) = req(
        &env.app,
        "POST",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&bob),
        Some(json!({"keyword": "finale", "whole_word": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{kw}");
    let kw_id = kw["id"].as_str().unwrap();
    let (status, kws) = req(
        &env.app,
        "GET",
        &format!("/api/v2/filters/{filter_id}/keywords"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kws.as_array().unwrap().len(), 2);
    let (status, _) = req(
        &env.app,
        "DELETE",
        &format!("/api/v2/filters/keywords/{kw_id}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete restores visibility.
    let (status, _) = req(
        &env.app,
        "DELETE",
        &format!("/api/v2/filters/{filter_id}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, home) = req(&env.app, "GET", "/api/v1/timelines/home", Some(&bob), None).await;
    let ids: Vec<&str> = home
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&matching["id"].as_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warn_filter_attaches_filtered_attr() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    let bob = register(&env.app, "bob", "b@x.test").await;
    let alice_id = sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE username = 'alice'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let (_, _) = req(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{alice_id}/follow"),
        Some(&bob),
        None,
    )
    .await;

    let post = post_status(&env.app, &alice, "thoughts about #rust today").await;
    let (_, _) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&bob),
        Some(json!({
            "title": "rust",
            "context": ["home"],
            "filter_action": "warn",
            "keywords_attributes": [{"keyword": "#rust", "whole_word": true}],
        })),
    )
    .await;

    let (_, home) = req(&env.app, "GET", "/api/v1/timelines/home", Some(&bob), None).await;
    let hit = home
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == post["id"])
        .expect("post still visible under warn");
    let filtered = hit["filtered"].as_array().unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["filter"]["filter_action"], "warn");
    assert_eq!(filtered[0]["keyword_matches"], json!(["#rust"]));
}

// ---------- tags ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_lookup_follow_and_home_delivery() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    let bob = register(&env.app, "bob", "b@x.test").await;

    post_status(&env.app, &alice, "loving #YonderTags right now").await;

    // Lookup is case-insensitive; history is present.
    let (status, tag) = req(&env.app, "GET", "/api/v1/tags/YonderTags", Some(&bob), None).await;
    assert_eq!(status, StatusCode::OK, "{tag}");
    assert_eq!(tag["name"], "yondertags");
    assert_eq!(tag["following"], false);
    assert_eq!(tag["history"].as_array().unwrap().len(), 7);

    // Unknown tag → 404.
    let (status, _) = req(&env.app, "GET", "/api/v1/tags/nosuchtag", Some(&bob), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Follow → `following` flips; followed_tags lists it.
    let (status, tag) = req(
        &env.app,
        "POST",
        "/api/v1/tags/yondertags/follow",
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tag}");
    assert_eq!(tag["following"], true);
    let (status, list) = req(&env.app, "GET", "/api/v1/followed_tags", Some(&bob), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap()[0]["name"], "yondertags");

    // Bob does NOT follow Alice — the tagged post still reaches home
    // through the tag follow (SQL revalidation is authoritative).
    let tagged = post_status(&env.app, &alice, "another #YonderTags post").await;
    let (_, home) = req(&env.app, "GET", "/api/v1/timelines/home", Some(&bob), None).await;
    let ids: Vec<&str> = home
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(
        ids.contains(&tagged["id"].as_str().unwrap()),
        "followed-tag post should reach home"
    );

    // Unfollow → `following` back to false.
    let (status, tag) = req(
        &env.app,
        "POST",
        "/api/v1/tags/yondertags/unfollow",
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag["following"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trends_tags_period_and_threshold() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    let bob = register(&env.app, "bob", "b@x.test").await;

    // `trends_min_accounts` defaults to 2 — a single author never trends.
    post_status(&env.app, &alice, "#trendme one").await;
    post_status(&env.app, &alice, "#trendme two").await;
    let (_, trends) = req(&env.app, "GET", "/api/v1/trends/tags", None, None).await;
    assert!(trends
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t["name"] != "trendme"));

    // Two distinct authors push it over the threshold.
    post_status(&env.app, &bob, "#trendme three").await;
    let (_, trends) = req(
        &env.app,
        "GET",
        "/api/v1/trends/tags?period=day",
        None,
        None,
    )
    .await;
    let hit = trends
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "trendme")
        .expect("tag should trend");
    assert!(hit["history"].as_array().unwrap().len() == 7);

    // Bad period → 422.
    let (status, _) = req(
        &env.app,
        "GET",
        "/api/v1/trends/tags?period=year",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------- emoji ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_upload_list_and_render() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    make_admin(&env, "alice").await;

    // Multipart upload.
    let (ct, body) = multipart("blobcat.png", &png(), &[("shortcode", "blobcat")]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let created: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["shortcode"], "blobcat");

    // Public catalogue lists it anonymously.
    let (status, list) = req(&env.app, "GET", "/api/v1/custom_emojis", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let hit = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["shortcode"] == "blobcat")
        .expect("emoji listed");
    assert!(hit["url"].as_str().unwrap().contains("/media/emoji/"));

    // `:blobcat:` renders as <img> in a new status and the entity
    // carries the emoji in `emojis`.
    let post = post_status(&env.app, &alice, "hello :blobcat: world").await;
    let content = post["content"].as_str().unwrap();
    assert!(
        content.contains("<img class=\"emoji\""),
        "content should embed the emoji image: {content}"
    );
    let emojis = post["emojis"].as_array().unwrap();
    assert!(emojis.iter().any(|e| e["shortcode"] == "blobcat"));

    // Duplicate shortcode → 409.
    let (ct, body) = multipart("blobcat.png", &png(), &[("shortcode", "blobcat")]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Non-admin → 403 on the admin surface.
    let bob = register(&env.app, "bob", "b@x.test").await;
    let (ct, body) = multipart("other.png", &png(), &[("shortcode", "other")]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {bob}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_zip_and_dir_import() {
    let mut config = Config::default();
    let import_root = std::env::temp_dir().join(format!("yonder-emoji-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(import_root.join("pack1")).unwrap();
    std::fs::write(import_root.join("pack1/dir_one.png"), png()).unwrap();
    config.admin.emoji_import_dir = Some(import_root.to_string_lossy().to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    make_admin(&env, "alice").await;

    // Zip import: two pngs + a non-image entry.
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("cat_one.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, &png()).unwrap();
        zip.start_file("cat_two.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, &png()).unwrap();
        zip.start_file("notes.txt", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"ignored").unwrap();
        zip.finish().unwrap();
    }
    let (ct, body) = multipart("emoji.zip", &buf, &[]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis/import_zip")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let out: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["imported"], 2, "{out}");

    // Filesystem import, non-recursive from `pack1`.
    let (status, out) = req(
        &env.app,
        "POST",
        "/api/v1/admin/emojis/import_dir",
        Some(&alice),
        Some(json!({"from_dir": "pack1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["imported"], 1, "{out}");

    // Path outside the root is refused.
    let (status, _) = req(
        &env.app,
        "POST",
        "/api/v1/admin/emojis/import_dir",
        Some(&alice),
        Some(json!({"from_dir": "../.."})),
    )
    .await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::NOT_FOUND,
        "escape must fail, got {status}"
    );

    // All three land in the public catalogue.
    let (_, list) = req(&env.app, "GET", "/api/v1/custom_emojis", None, None).await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["shortcode"].as_str())
        .collect();
    for code in ["cat_one", "cat_two", "dir_one"] {
        assert!(names.contains(&code), "missing {code} in {names:?}");
    }
}

// ---------- review pass 1 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_hide_list_and_show() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_n", "alice_n@x.test").await;
    let bob = register(&env.app, "bob_n", "bob_n@x.test").await;
    let s = post_status(&env.app, &bob, "hello spamworld").await;
    let sid: i64 = s["id"].as_str().unwrap().parse().unwrap();
    let (st, _) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&alice),
        Some(json!({
            "title": "f", "context": ["notifications"], "filter_action": "hide",
            "keywords_attributes": [{"keyword": "spamworld"}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let bob_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'bob_n'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let alice_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'alice_n'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let nid: i64 = sqlx::query_scalar(
        "INSERT INTO notifications (account_id, from_account_id, type, status_id) \
         VALUES ($1,$2,'favourite',$3) RETURNING id",
    )
    .bind(alice_id)
    .bind(bob_id)
    .bind(sid)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (st, body) = req(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty(), "{body}");
    let (st, _) = req(
        &env.app,
        "GET",
        &format!("/api/v1/notifications/{nid}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_form_encoded_create() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "fe_user", "fe@x.test").await;
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v2/filters")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    "title=My+filter&context[]=home&context[]=notifications&\
                     filter_action=hide&keywords_attributes[0][keyword]=bad+stuff&\
                     keywords_attributes[0][whole_word]=true",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "My filter");
    assert_eq!(body["filter_action"], "hide");
    assert_eq!(body["keywords"].as_array().unwrap().len(), 1);
    assert_eq!(body["keywords"][0]["keyword"], "bad stuff");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_status_pin_requires_visibility() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let a = register(&env.app, "pin_a", "pa@x.test").await;
    let b = register(&env.app, "pin_b", "pb@x.test").await;
    let (st, f) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&b),
        Some(json!({"title": "f", "context": ["home"]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let fid = f["id"].as_str().unwrap();
    // Nonexistent status → 404.
    let (st, _) = req(
        &env.app,
        "POST",
        &format!("/api/v2/filters/{fid}/statuses"),
        Some(&b),
        Some(json!({"status_id": "999999"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // A followers_only post B can't see → 404.
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/statuses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {a}"))
                .body(Body::from(
                    json!({"status": "secret", "visibility": "private"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let s: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let (st, _) = req(
        &env.app,
        "POST",
        &format!("/api/v2/filters/{fid}/statuses"),
        Some(&b),
        Some(json!({"status_id": s["id"]})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // …but A can pin their own post.
    let (st, f2) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&a),
        Some(json!({"title": "f", "context": ["home"]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = req(
        &env.app,
        "POST",
        &format!("/api/v2/filters/{}/statuses", f2["id"].as_str().unwrap()),
        Some(&a),
        Some(json!({"status_id": s["id"]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_zip_caps_and_safe_overwrite() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_z", "az@x.test").await;
    make_admin(&env, "alice_z").await;

    // Seed :keepme: via the single-upload route.
    let (ct, body) = multipart("keepme.png", &png(), &[("shortcode", "keepme")]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A zip over the entry-count cap fails 422 before any insert.
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        for i in 0..=500 {
            zip.start_file(format!("e{i}.png"), opts).unwrap();
            std::io::Write::write_all(&mut zip, &png()).unwrap();
        }
        zip.finish().unwrap();
    }
    let (ct, body) = multipart("big.zip", &buf, &[]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis/import_zip")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let (st, list) = req(&env.app, "GET", "/api/v1/custom_emojis", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["shortcode"].as_str().unwrap_or_default().starts_with('e')),
        "no partial inserts: {list}"
    );

    // overwrite=true with a corrupt replacement keeps the old emoji.
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("keepme.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"not an image").unwrap();
        zip.finish().unwrap();
    }
    let (ct, body) = multipart("ow.zip", &buf, &[("overwrite", "true")]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis/import_zip")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (st, list) = req(&env.app, "GET", "/api/v1/custom_emojis", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|e| e["shortcode"] == "keepme"),
        ":keepme: must survive a failed overwrite: {list}"
    );
}

// ---------- review pass 2 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_patch_clears_nullable_fields() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_p", "ap@x.test").await;
    make_admin(&env, "alice_p").await;

    let (ct, body) = multipart(
        "patchme.png",
        &png(),
        &[("shortcode", "patchme"), ("category", "memes")],
    );
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let emoji_id: i64 =
        sqlx::query_scalar("SELECT id FROM custom_emojis WHERE shortcode = 'patchme'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    let (st, pack) = req(
        &env.app,
        "POST",
        "/api/v1/admin/emoji_packs",
        Some(&alice),
        Some(json!({"name": "stickers"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{pack}");
    let pack_id: i64 = pack["id"].as_str().unwrap().parse().unwrap();

    // Set both, then clear both with explicit JSON null.
    let (st, body) = req(
        &env.app,
        "PATCH",
        &format!("/api/v1/admin/emojis/{emoji_id}"),
        Some(&alice),
        Some(json!({"category": "reactions", "pack_id": pack_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = req(
        &env.app,
        "PATCH",
        &format!("/api/v1/admin/emojis/{emoji_id}"),
        Some(&alice),
        Some(json!({"category": null, "pack_id": null})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body["category"].is_null(), "category must clear: {body}");
    let db_pack: Option<i64> =
        sqlx::query_scalar("SELECT pack_id FROM custom_emojis WHERE id = $1")
            .bind(emoji_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(db_pack.is_none(), "pack_id must clear to NULL");

    // Invalid multipart pack_id and over-long category are 422s.
    let (ct, body) = multipart(
        "badpack.png",
        &png(),
        &[("shortcode", "badpack"), ("pack_id", "abc")],
    );
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = req(
        &env.app,
        "PATCH",
        &format!("/api/v1/admin/emojis/{emoji_id}"),
        Some(&alice),
        Some(json!({"category": "x".repeat(101)})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_zip_reports_skipped_and_refused_entries() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_r", "ar@x.test").await;
    make_admin(&env, "alice_r").await;

    // 2 valid PNGs + a text file + an absolute-path name: every file
    // entry gets a results row — junk is skipped, `/abs.png` refused.
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("ok_one.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, &png()).unwrap();
        zip.start_file("ok_two.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, &png()).unwrap();
        zip.start_file("notes.txt", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"junk").unwrap();
        zip.start_file("/abs.png", opts).unwrap();
        std::io::Write::write_all(&mut zip, &png()).unwrap();
        zip.finish().unwrap();
    }
    let (ct, body) = multipart("mixed.zip", &buf, &[]);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/admin/emojis/import_zip")
                .header("content-type", ct)
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let out: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["imported"], 2, "{out}");
    assert_eq!(out["skipped"], 2, "{out}");
    let results = out["results"].as_array().unwrap();
    let junk = results
        .iter()
        .find(|r| r["name"] == "notes.txt")
        .expect("junk entry reported");
    assert_eq!(junk["ok"], false);
    assert!(
        junk["error"].as_str().unwrap().contains("not an image"),
        "{junk}"
    );
    let abs = results
        .iter()
        .find(|r| r["name"] == "/abs.png")
        .expect("absolute entry reported");
    assert_eq!(abs["ok"], false);
    assert!(abs["error"].as_str().unwrap().contains("absolute"), "{abs}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_form_groups_keywords_by_index() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "fe_user2", "fe2@x.test").await;
    // Interleaved indices: [1]'s keyword sits between [0]'s fields —
    // the flags must still land on their own index (absent
    // `whole_word` defaults to true, Mastodon-style).
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v2/filters")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    "title=Grouped&context[]=home&\
                     keywords_attributes[0][keyword]=aaa&\
                     keywords_attributes[1][keyword]=bbb&\
                     keywords_attributes[0][whole_word]=true&\
                     keywords_attributes[1][whole_word]=false",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let kws = body["keywords"].as_array().unwrap();
    assert_eq!(kws.len(), 2, "{body}");
    assert_eq!(kws[0]["keyword"], "aaa");
    assert_eq!(kws[0]["whole_word"], true);
    assert_eq!(kws[1]["keyword"], "bbb");
    assert_eq!(kws[1]["whole_word"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_malformed_json_is_400() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "badjson", "bj@x.test").await;
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v2/filters")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{not valid json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notification_hide_excluded_from_unread_count() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_c", "ac@x.test").await;
    let bob = register(&env.app, "bob_c", "bc@x.test").await;
    let hidden_status = post_status(&env.app, &bob, "spammy offer inside").await;
    let clean_status = post_status(&env.app, &bob, "perfectly fine post").await;
    let (st, _) = req(
        &env.app,
        "POST",
        "/api/v2/filters",
        Some(&alice),
        Some(json!({
            "title": "f", "context": ["notifications"], "filter_action": "hide",
            "keywords_attributes": [{"keyword": "spammy"}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let bob_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'bob_c'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let alice_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'alice_c'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    for s in [&hidden_status, &clean_status] {
        let sid: i64 = s["id"].as_str().unwrap().parse().unwrap();
        sqlx::query(
            "INSERT INTO notifications (account_id, from_account_id, type, status_id) \
             VALUES ($1,$2,'favourite',$3)",
        )
        .bind(alice_id)
        .bind(bob_id)
        .bind(sid)
        .execute(&env.state.db)
        .await
        .unwrap();
    }
    // The hidden row's status matches the rule — only the clean one counts.
    let (st, body) = req(
        &env.app,
        "GET",
        "/api/v1/notifications/unread_count",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["count"], 1, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trends_offset_pages_within_ceiling() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice_t", "at@x.test").await;
    let bob = register(&env.app, "bob_t", "bt@x.test").await;
    // Two tags over the two-author floor; equal stats → name order.
    for user in [&alice, &bob] {
        post_status(&env.app, user, "#aaa_trend post").await;
        post_status(&env.app, user, "#bbb_trend post").await;
    }
    let (_, page1) = req(
        &env.app,
        "GET",
        "/api/v1/trends/tags?period=day&limit=1",
        None,
        None,
    )
    .await;
    assert_eq!(page1.as_array().unwrap()[0]["name"], "aaa_trend", "{page1}");
    let (_, page2) = req(
        &env.app,
        "GET",
        "/api/v1/trends/tags?period=day&limit=1&offset=1",
        None,
        None,
    )
    .await;
    assert_eq!(page2.as_array().unwrap()[0]["name"], "bbb_trend", "{page2}");
    let (_, page3) = req(
        &env.app,
        "GET",
        "/api/v1/trends/tags?period=day&limit=1&offset=2",
        None,
        None,
    )
    .await;
    assert!(page3.as_array().unwrap().is_empty(), "{page3}");
}
