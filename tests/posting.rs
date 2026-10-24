//! Epic-12 posting integration tests — create/render, visibility,
//! idempotency, polls, scheduling, media and remote materialization
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
    posts::{self, jobs::StatusDeletionJob, remote, PostContext},
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
    config.rate_limit.status = config.rate_limit.authentication.clone();
    config.rate_limit.media = config.rate_limit.authentication.clone();
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-post-{}", uuid::Uuid::new_v4()))
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
        _pg: pg,
        _redis: redis,
    })
}

/// Register a user, returning its bearer token.
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
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
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

fn ctx(state: &AppState) -> PostContext {
    PostContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        dispatcher: yonder::jobs::Dispatcher::new(&state.redis, &state.config().jobs),
        config: state.config().clone(),
        media: state.media.clone(),
        ap: state.ap.clone(),
        bus: state.bus.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_show_plain_status() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "alice", "alice@example.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "hello https://example.com #rust"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap();
    let html = body["content"].as_str().unwrap();
    assert!(html.contains("href=\"https://example.com\""), "{html}");
    assert!(html.contains("rel=\"tag\""), "{html}");
    assert_eq!(body["tags"][0]["name"], "rust");
    assert_eq!(body["visibility"], "public");

    // Anonymous read of a public status.
    let (status, shown) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["id"], id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_renders_and_sanitizes() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "bob", "bob@example.com").await;
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "# Title\n\n**bold** <script>alert(1)</script>",
            "content_type": "text/markdown",
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let html = body["content"].as_str().unwrap();
    assert!(html.contains("<strong>bold</strong>"), "{html}");
    assert!(!html.contains("<script>"), "{html}");
    assert_eq!(body["content_type"], "text/markdown");
    assert_eq!(
        body["yonder"]["source"],
        "# Title\n\n**bold** <script>alert(1)</script>"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visibility_gates_reads() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "carol", "carol@example.com").await;
    let dave = register(&env.app, "dave", "dave@example.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "secret", "visibility": "private"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Stored as followers_only; wire output maps back to private.
    assert_eq!(body["visibility"], "private");
    let id = body["id"].as_str().unwrap();

    // Another user cannot see it.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        Some(&dave),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Anonymous cannot see it.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The author can.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_replays_result() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "erin", "erin@example.com").await;
    let headers = [("idempotency-key", "key-1")];

    let (s1, b1) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "once"})),
        &headers,
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "{b1}");
    let (s2, b2) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "once"})),
        &headers,
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "{b2}");
    assert_eq!(b1["id"], b2["id"]);

    // Same key, different payload → 422.
    let (s3, _) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "different"})),
        &headers,
    )
    .await;
    assert_eq!(s3, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polls_create_vote_and_reject_duplicates() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let author = register(&env.app, "fred", "fred@example.com").await;
    let voter = register(&env.app, "gwen", "gwen@example.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&author),
        Some(json!({
            "status": "pick",
            "poll": {"options": ["a", "b"], "expires_in": 3600},
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let poll_id = body["poll"]["id"].as_str().unwrap();

    let (status, poll) = request(
        &env.app,
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&voter),
        Some(json!({"choices": [1]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{poll}");
    assert_eq!(poll["votes_count"], 1);
    assert_eq!(poll["options"][1]["votes_count"], 1);
    assert_eq!(poll["own_votes"], json!([1]));

    // Duplicate vote → 422.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&voter),
        Some(json!({"choices": [0]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Own-poll vote → 422.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&author),
        Some(json!({"choices": [0]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Out-of-bounds choice → 422.
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&voter),
        Some(json!({"choices": [5]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_status_crud_and_due_guard() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "hank", "hank@example.com").await;
    let when = chrono::Utc::now() + chrono::Duration::hours(1);

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "later", "scheduled_at": when.to_rfc3339()})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sid = body["id"].as_str().unwrap();
    assert!(body["scheduled_at"].is_string());
    assert_eq!(body["params"]["status"], "later");

    // Due-time guard: running the publish job now must NOT publish.
    let c = ctx(&env.state);
    posts::scheduled::ScheduledStatusPublishJob::new(c.ap.clone())
        .run(
            &JobContext {
                db: env.state.db.clone(),
                redis: env.state.redis.clone(),
                config: env.state.config().clone(),
                media: env.state.media.clone(),
            },
            &json!({"scheduled_status_id": sid.parse::<i64>().unwrap()}),
        )
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE content = 'later'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "early publish must be refused");

    // Reschedule to the past → sweep publishes it.
    let sid_i: i64 = sid.parse().unwrap();
    sqlx::query(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(sid_i)
    .execute(&env.state.db)
    .await
    .unwrap();
    posts::scheduled::ScheduledStatusPublishJob::new(c.ap.clone())
        .run(
            &JobContext {
                db: env.state.db.clone(),
                redis: env.state.redis.clone(),
                config: env.state.config().clone(),
                media: env.state.media.clone(),
            },
            &json!({"scheduled_status_id": sid_i}),
        )
        .await
        .unwrap();
    let row: Option<i64> = sqlx::query_scalar("SELECT id FROM statuses WHERE content = 'later'")
        .fetch_optional(&env.state.db)
        .await
        .unwrap();
    assert!(row.is_some(), "due scheduled status must publish");

    // Cancel path: a fresh scheduled status deletes cleanly.
    let (_, b) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "cancel me", "scheduled_at": when.to_rfc3339()})),
        &[],
    )
    .await;
    let (status, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/scheduled_statuses/{}", b["id"].as_str().unwrap()),
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_and_delete_status() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ivy", "ivy@example.com").await;
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "first"})),
        &[],
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["edited_at"].is_string());
    assert_eq!(body["yonder"]["source"], "edited");

    let (status, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Tombstoned: a GET now 404s.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_note_materializes() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/1"),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": "https://remote.test/objects/1",
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
    let handled = remote::handle(&core, &activity, &activity.to_json()).await;
    assert!(handled);
    let row: Option<(bool, String)> = sqlx::query_as(
        "SELECT local, visibility FROM statuses WHERE uri = 'https://remote.test/objects/1'",
    )
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    let (local, vis) = row.expect("remote status materialized");
    assert!(!local);
    assert_eq!(vis, "public");

    // Spoofed Update — actor differs from the stored owner.
    let spoofed = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/2"),
        activity_type: "Update".into(),
        actor: "https://evil.test/users/mallory".into(),
        object: json!({
            "id": "https://remote.test/objects/1",
            "type": "Note",
            "attributedTo": "https://evil.test/users/mallory",
            "content": "<p>hacked</p>",
            "updated": "2025-02-01T00:00:00Z",
        }),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    remote::handle(&core, &spoofed, &spoofed.to_json()).await;
    let html: String = sqlx::query_scalar(
        "SELECT content_html FROM statuses WHERE uri = 'https://remote.test/objects/1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!html.contains("hacked"), "spoofed update must be dropped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_deletion_tombstones() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "jane", "jane@example.com").await;
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "delete me soon"})),
        &[],
    )
    .await;
    let id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    sqlx::query(
        "UPDATE statuses SET scheduled_deletion_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(id)
    .execute(&env.state.db)
    .await
    .unwrap();
    StatusDeletionJob::new(ctx(&env.state).ap.clone())
        .run(
            &JobContext {
                db: env.state.db.clone(),
                redis: env.state.redis.clone(),
                config: env.state.config().clone(),
                media: env.state.media.clone(),
            },
            &json!({"status_id": id}),
        )
        .await
        .unwrap();
    let tombstone: bool = sqlx::query_scalar("SELECT tombstone FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(tombstone);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_entity_includes_yonder_fields() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "kate", "kate@example.com").await;
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "hi", "spoiler_text": "cw", "sensitive": true})),
        &[],
    )
    .await;
    assert_eq!(body["spoiler_text"], "cw");
    assert_eq!(body["sensitive"], true);
    assert!(body["yonder"]["source"].is_string());
    assert!(body["uri"].is_string());
    assert!(body["url"].is_string());
}

// ---------------------------------------------------------------------------
// Review-fix regression tests
// ---------------------------------------------------------------------------

/// A tiny valid PNG, generated via the `image` crate.
fn png_bytes() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
    let mut cur = std::io::Cursor::new(Vec::new());
    img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
    cur.into_inner()
}

/// POST a multipart body (`file` + extra text fields) like a real client.
async fn multipart(
    app: &Router,
    path: &str,
    token: &str,
    file: &[u8],
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    let boundary = "TESTBOUNDARY42";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.png\"\r\n\
             Content-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body))
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

fn job_ctx(env: &Env) -> JobContext {
    JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polls_respect_status_visibility() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let author = register(&env.app, "lisa", "lisa@example.com").await;
    let outsider = register(&env.app, "mike", "mike@example.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&author),
        Some(json!({
            "status": "followers poll",
            "visibility": "private",
            "poll": {"options": ["a", "b"], "expires_in": 3600},
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let poll_id = body["poll"]["id"].as_str().unwrap();

    // A non-follower cannot read or vote the poll.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/polls/{poll_id}"),
        Some(&outsider),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/polls/{poll_id}/votes"),
        Some(&outsider),
        Some(json!({"choices": [0]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The author can still read it.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/polls/{poll_id}"),
        Some(&author),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_upload_focus_and_attachment() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "nina", "nina@example.com").await;

    // Upload with alt text + focus.
    let (status, media) = multipart(
        &env.app,
        "/api/v1/media",
        &token,
        &png_bytes(),
        &[("description", "a square"), ("focus", "0.5,-0.5")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{media}");
    assert_eq!(media["type"], "image");
    assert_eq!(media["description"], "a square");
    assert_eq!(media["meta"]["focus"]["x"], 0.5);
    assert_eq!(media["meta"]["focus"]["y"], -0.5);
    assert!(media["meta"]["original"]["width"].is_number());
    assert!(media["blurhash"].is_string());
    let media_id = media["id"].as_str().unwrap();

    // Out-of-range focus → 422.
    let (status, _) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        Some(json!({"focus": {"x": 2.0, "y": 0.0}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // In-range focus updates.
    let (status, updated) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/media/{media_id}"),
        Some(&token),
        Some(json!({"focus": {"x": -1.0, "y": 1.0}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["meta"]["focus"]["x"], -1.0);

    // Attach to a status; the entity carries the media.
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "with media", "media_ids": [media_id]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_attachments"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["media_attachments"][0]["id"].as_str().unwrap(),
        media_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_object_get_returns_404() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "olga", "olga@example.com").await;

    // Private status: unsigned object fetch must not reveal it exists.
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "followers only", "visibility": "private"})),
        &[],
    )
    .await;
    let id = body["id"].as_str().unwrap();
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/ap/users/olga/statuses/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Public status: unsigned fetch serves the object.
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "hello world", "visibility": "public"})),
        &[],
    )
    .await;
    let id = body["id"].as_str().unwrap();
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/ap/users/olga/statuses/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["type"], "Note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_cleanup_respects_scheduled_reservation() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "pete", "pete@example.com").await;
    let account_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'pete' AND domain IS NULL")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    let stored = yonder::media::store_status_media(
        &env.state.config(),
        &env.state.media,
        &env.state.db,
        account_id,
        png_bytes(),
        Default::default(),
    )
    .await
    .unwrap();

    // Reserve the media via a scheduled status.
    let when = chrono::Utc::now() + chrono::Duration::hours(2);
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "scheduled with media",
            "media_ids": [stored.id],
            "scheduled_at": when.to_rfc3339(),
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let scheduled_id = body["id"].as_str().unwrap().to_string();

    // Age the upload well past retention — the reservation protects it.
    sqlx::query(
        "UPDATE media_attachments SET created_at = now() - interval '30 days' WHERE id = $1",
    )
    .bind(stored.id)
    .execute(&env.state.db)
    .await
    .unwrap();
    posts::jobs::CleanupOrphanMediaJob
        .run(&job_ctx(&env), &json!({}))
        .await
        .unwrap();
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_attachments WHERE id = $1)")
            .bind(stored.id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(exists, "scheduled reservation must protect the upload");

    // Cancel the schedule; the next sweep reclaims the upload.
    let (status, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/scheduled_statuses/{scheduled_id}"),
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    posts::jobs::CleanupOrphanMediaJob
        .run(&job_ctx(&env), &json!({}))
        .await
        .unwrap();
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_attachments WHERE id = $1)")
            .bind(stored.id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!exists, "unreserved orphan must be cleaned");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_replaces_poll() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "quinn", "quinn@example.com").await;
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "v1",
            "poll": {"options": ["a", "b"], "expires_in": 3600},
        })),
        &[],
    )
    .await;
    let id = body["id"].as_str().unwrap();
    let old_poll = body["poll"]["id"].as_str().unwrap().to_string();

    // Editing with a new poll replaces the old one.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({
            "status": "v2",
            "poll": {"options": ["x", "y", "z"], "expires_in": 7200},
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_poll = body["poll"]["id"].as_str().unwrap();
    assert_ne!(new_poll, old_poll);
    assert_eq!(body["poll"]["options"].as_array().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_media_semantics() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "rosa", "rosa@example.com").await;

    let (status, media) = multipart(&env.app, "/api/v1/media", &token, &png_bytes(), &[]).await;
    assert_eq!(status, StatusCode::OK, "{media}");
    let media_id = media["id"].as_str().unwrap();

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "with media", "media_ids": [media_id]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap();
    assert_eq!(body["media_attachments"].as_array().unwrap().len(), 1);

    // Omitting media_ids keeps the existing attachments.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited, media omitted"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["media_attachments"].as_array().unwrap().len(),
        1,
        "omitted media_ids must keep attachments: {body}"
    );

    // Resubmitting the already-attached id keeps it too.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited, media resubmitted", "media_ids": [media_id]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["media_attachments"].as_array().unwrap().len(),
        1,
        "resubmitted media_ids must stay attached: {body}"
    );

    // An explicit empty list removes them.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited, media cleared", "media_ids": []})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["media_attachments"].as_array().unwrap().is_empty(),
        "explicit empty media_ids must clear attachments: {body}"
    );
}
