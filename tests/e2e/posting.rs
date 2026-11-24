//! Epic-12 posting integration tests — create/render, visibility,
//! idempotency, polls, scheduling, media and remote materialization
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
use redis::AsyncCommands;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{queue, JobContext, JobEnvelope, JobHandler, JobKind, Queue},
    posts::{self, jobs::StatusDeletionJob, preview::FetchLinkPreviewJob, remote, PostContext},
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
        client_app: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_show_plain_status() {
    let _permit = harness::acquire().await;
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

    // The author's `statuses_count` moved with the insert.
    let count: i32 = sqlx::query_scalar("SELECT statuses_count FROM accounts WHERE id = $1")
        .bind(
            body["account"]["id"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap(),
        )
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// Mona and several other mobile clients post statuses as
/// `multipart/form-data`; field names mirror the urlencoded form
/// keys, so a multipart `status` must land in the same param shape.
/// (Regression: multipart fell through to the JSON branch, parsed as
/// `{}`, and the post was rejected with "status cannot be empty".)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_status_multipart_form_data() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "mpart", "mpart@example.com").await;

    let boundary = "yonder-test-boundary";
    let body = concat!(
        "--{b}\r\nContent-Disposition: form-data; name=\"status\"\r\n\r\n",
        "hello from multipart #multipart\r\n",
        "--{b}\r\nContent-Disposition: form-data; name=\"visibility\"\r\n\r\n",
        "unlisted\r\n",
        "--{b}--\r\n",
    )
    .replace("{b}", boundary);
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/statuses")
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
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["visibility"], "unlisted");
    assert_eq!(body["tags"][0]["name"], "multipart");
    assert!(body["content"].as_str().unwrap().contains("multipart"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_renders_and_sanitizes() {
    let _permit = harness::acquire().await;
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
async fn markdown_inline_image_renders_and_federates_as_attachment() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ima", "ima@example.com").await;
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "look ![A tiny png](https://img.example/pic.png) here",
            "content_type": "text/markdown",
            "visibility": "public",
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let html = body["content"].as_str().unwrap();
    assert!(
        html.contains("<img src=\"https://img.example/pic.png\" alt=\"A tiny png\">"),
        "{html}"
    );
    assert!(
        html.contains(
            "data-yonder-inline-image-alt><a href=\"https://img.example/pic.png\">A tiny png</a></small>"
        ),
        "{html}"
    );
    // Inline images are content, not uploaded media rows.
    assert_eq!(body["media_attachments"], json!([]));

    // The AP object still delivers the image as a standard attachment
    // for implementations without inline-image support.
    let id = body["id"].as_str().unwrap();
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/ap/users/ima/statuses/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    let attachment = doc["attachment"]
        .as_array()
        .and_then(|a| a.iter().find(|d| d["url"] == "https://img.example/pic.png"))
        .cloned()
        .expect("inline image attachment");
    assert_eq!(attachment["type"], "Image");
    assert_eq!(attachment["mediaType"], "image/png");
    assert_eq!(attachment["name"], "A tiny png");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_default_content_type_applies_when_unspecified() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "md", "md@example.com").await;

    // Fresh account: the instance default (text/plain) applies.
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**not bold**"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content_type"], "text/plain");

    // The account preference wins over the instance default.
    let (status, body) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&token),
        Some(json!({"default_content_type": "text/markdown"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_content_type"], "text/markdown");
    assert_eq!(body["effective_content_type"], "text/markdown");

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**bold**"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content_type"], "text/markdown");
    assert!(
        body["content"]
            .as_str()
            .unwrap()
            .contains("<strong>bold</strong>"),
        "{body}"
    );

    // An explicit per-post content_type still wins over the preference.
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "**not bold**", "content_type": "text/plain"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content_type"], "text/plain");
    assert!(
        !body["content"].as_str().unwrap().contains("<strong>"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visibility_gates_reads() {
    let _permit = harness::acquire().await;
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
async fn local_visibility_is_instance_scoped() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "instance only", "visibility": "local"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "local");
    let id = body["id"].as_str().unwrap().to_string();
    let id_num: i64 = id.parse().unwrap();

    // Any other account logged on this instance can read it.
    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        Some(&bob),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Anonymous cannot.
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

    // The local timeline shows it to authenticated viewers only.
    let local_tl = "/api/v1/timelines/public?local=true";
    let has_id = |body: &Value| {
        body.as_array()
            .is_some_and(|rows| rows.iter().any(|s| s["id"].as_str() == Some(id.as_str())))
    };
    let (_, body) = request(&env.app, "GET", local_tl, Some(&alice), None, &[]).await;
    assert!(has_id(&body), "{body}");
    let (_, body) = request(&env.app, "GET", local_tl, Some(&bob), None, &[]).await;
    assert!(has_id(&body), "{body}");
    let (_, body) = request(&env.app, "GET", local_tl, None, None, &[]).await;
    assert!(!has_id(&body), "{body}");

    // The federated timeline never carries it.
    let (_, body) = request(
        &env.app,
        "GET",
        "/api/v1/timelines/public",
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert!(!has_id(&body), "{body}");

    // Nothing federated: no `activities` row for the status's object.
    let activities: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activities WHERE object_id = \
         (SELECT uri FROM statuses WHERE id = $1)",
    )
    .bind(id_num)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(activities, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotency_replays_result() {
    let _permit = harness::acquire().await;
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
async fn stale_idempotency_reservation_is_taken_over() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ida", "ida@example.com").await;
    let headers = [("idempotency-key", "stale-key")];
    let body = || json!({"status": "retry me"});

    // Complete a real request so the stored fingerprint is genuine.
    let (s, b) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(body()),
        &headers,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // Simulate a request that died mid-flight: reservation kept, target
    // never back-filled. A *fresh* stuck row must still 409.
    sqlx::query(
        "UPDATE status_idempotency_keys SET target_id = NULL, created_at = now() \
         WHERE idempotency_key = 'stale-key'",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    let (s, _) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(body()),
        &headers,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Age the stuck reservation past the stale threshold — the retry
    // takes it over and posts a fresh status instead of 409ing until
    // the 24 h prune.
    sqlx::query(
        "UPDATE status_idempotency_keys SET created_at = now() - interval '1 hour' \
         WHERE idempotency_key = 'stale-key'",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    let (s, b2) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(body()),
        &headers,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b2}");
    assert_ne!(b["id"], b2["id"], "takeover must create a new status");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn polls_create_vote_and_reject_duplicates() {
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let account_id: i64 = body["account"]["id"].as_str().unwrap().parse().unwrap();

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
    // The author's `statuses_count` released with the tombstone.
    let count: i32 = sqlx::query_scalar("SELECT statuses_count FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_note_materializes() {
    let _permit = harness::acquire().await;
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

/// FEP-8a8e ownership gate: an Event may be signed by an alternate
/// same-host actor only when `attributedTo` is a collective on the
/// object's own host. Cross-host attribution, known `Person`
/// attribution, and signers absent from `actor`/`organizers` must all
/// be dropped; an unknown same-host `attributedTo` is allowed on
/// first contact while its actor document is fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_ownership_gate() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let ingest = |n: usize, actor: &str, object: Value| {
        let core = core.clone();
        let actor = actor.to_string();
        async move {
            let activity = pubby_sdk::Activity {
                id: format!("{BASE}/ap/activities/evt{n}"),
                activity_type: "Create".into(),
                actor,
                object,
                to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
                cc: vec![],
                published: None,
                signature: None,
            };
            remote::handle(&core, &activity, &activity.to_json()).await
        }
    };
    let status_exists = |uri: &str| {
        let db = env.state.db.clone();
        let uri = uri.to_string();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM statuses WHERE uri = $1)")
                .bind(uri)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let event = |id: &str, attributed: Value, signer: &str| {
        json!({
            "id": id,
            "type": "Event",
            "name": "Gate test",
            "attributedTo": attributed,
            "actor": signer,
            "startTime": "2026-03-01T10:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };

    // Collective attributedTo on the object's host, signer listed as
    // the object `actor` → accepted, stored under the group.
    // (`handle` returns true for handled-and-ignored alike; the
    // status row is the source of truth.)
    ingest(
        1,
        "https://remote.test/@alice",
        event(
            "https://remote.test/events/1",
            json!({"id": "https://remote.test/@club", "type": "Group"}),
            "https://remote.test/@alice",
        ),
    )
    .await;
    assert!(status_exists("https://remote.test/events/1").await);
    let owner: String = sqlx::query_scalar(
        "SELECT a.actor_id FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE s.uri = 'https://remote.test/events/1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(owner, "https://remote.test/@club");
    // The events side table materialized too.
    let is_event: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM events e JOIN statuses s ON s.id = e.status_id \
         WHERE s.uri = 'https://remote.test/events/1')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(is_event);

    // attributedTo on a *different* host → rejected even though the
    // signer shares the object's host.
    ingest(
        2,
        "https://remote.test/@alice",
        event(
            "https://remote.test/events/2",
            json!({"id": "https://other.test/@club", "type": "Group"}),
            "https://remote.test/@alice",
        ),
    )
    .await;
    assert!(!status_exists("https://remote.test/events/2").await);

    // Known `Person` attributedTo, alternate same-host signer →
    // rejected (the embedded type hint is authoritative).
    ingest(
        3,
        "https://remote.test/@alice",
        event(
            "https://remote.test/events/3",
            json!({"id": "https://remote.test/@bob", "type": "Person"}),
            "https://remote.test/@alice",
        ),
    )
    .await;
    assert!(!status_exists("https://remote.test/events/3").await);

    // Signer absent from `actor`/`organizers` → rejected even with a
    // collective attributedTo.
    ingest(
        4,
        "https://remote.test/@mallory",
        event(
            "https://remote.test/events/4",
            json!({"id": "https://remote.test/@club2", "type": "Group"}),
            "https://remote.test/@alice",
        ),
    )
    .await;
    assert!(!status_exists("https://remote.test/events/4").await);

    // Unknown same-host attributedTo (bare URI, no type anywhere) →
    // allowed on first contact; a stub account is created so a later
    // fetch resolves the type.
    ingest(
        5,
        "https://remote.test/@alice",
        event(
            "https://remote.test/events/5",
            json!("https://remote.test/@carol"),
            "https://remote.test/@alice",
        ),
    )
    .await;
    assert!(status_exists("https://remote.test/events/5").await);
    let stub: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = 'https://remote.test/@carol')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(stub, "unknown attributedTo gets a stub for actor fetch");
}

/// A remote event's AS2 `summary` — the machine-generated
/// when/where/description preamble Yonder and Gancio emit for
/// Mastodon-style clients — must never fold into `spoiler_text` (the
/// event card renders the structured fields itself), while ordinary
/// notes keep the summary-as-CW behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_summary_never_becomes_cw() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let ingest = |n: usize, object: Value| {
        let core = core.clone();
        async move {
            let activity = pubby_sdk::Activity {
                id: format!("{BASE}/ap/activities/cwtest{n}"),
                activity_type: "Create".into(),
                actor: "https://remote.test/@org".into(),
                object,
                to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
                cc: vec![],
                published: None,
                signature: None,
            };
            remote::handle(&core, &activity, &activity.to_json()).await
        }
    };
    let preamble = "<p>When: 2026-03-01 10:00 (UTC)<br>Where: Somewhere</p><p>A test event.</p>";

    // Event with the Yonder-style preamble summary → no CW, but the
    // summary column keeps it for search.
    ingest(
        1,
        json!({
            "id": "https://remote.test/events/cw1",
            "type": "Event",
            "name": "CW test",
            "attributedTo": "https://remote.test/@org",
            "startTime": "2026-03-01T10:00:00Z",
            "content": "<p>A test event.</p>",
            "summary": preamble,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    )
    .await;
    let (spoiler, summary): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT spoiler_text, summary FROM statuses WHERE uri = 'https://remote.test/events/cw1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(spoiler, None, "event preamble must not become a CW");
    assert!(
        summary.is_some(),
        "summary column keeps the text for search"
    );

    // Control: a plain note's summary still becomes the CW.
    ingest(
        2,
        json!({
            "id": "https://remote.test/notes/cw2",
            "type": "Note",
            "attributedTo": "https://remote.test/@org",
            "content": "<p>spooky body</p>",
            "summary": "spoilers ahead",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    )
    .await;
    let note_spoiler: Option<String> = sqlx::query_scalar(
        "SELECT spoiler_text FROM statuses WHERE uri = 'https://remote.test/notes/cw2'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(note_spoiler.as_deref(), Some("spoilers ahead"));
}

/// FEP-8a8e `Invite{Event}` handling: organizer invites write
/// `invited` participation rows for local addressees; non-organizers,
/// remote-only addressees, and non-Event objects are rejected or
/// ignored; decided participation states are never clobbered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_invites() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let _token = register(&env.app, "alice", "alice@x.com").await;
    let alice_actor = format!("{BASE}/ap/users/alice");

    // Remote event materialized by its organizer.
    let create = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/ev100"),
        activity_type: "Create".into(),
        actor: "https://remote.test/@org".into(),
        object: json!({
            "id": "https://remote.test/events/100",
            "type": "Event",
            "name": "Invite test",
            "attributedTo": "https://remote.test/@org",
            "startTime": "2026-03-01T10:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &create, &create.to_json()).await);

    let invite = |n: usize, actor: &str, object: Value, to: Vec<String>| pubby_sdk::Activity {
        id: format!("https://remote.test/ap/activities/inv{n}"),
        activity_type: "Invite".into(),
        actor: actor.into(),
        object,
        to,
        cc: vec![],
        published: None,
        signature: None,
    };
    let participation = || {
        let db = env.state.db.clone();
        let actor = alice_actor.clone();
        async move {
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT p.state, p.protocol, p.invite_activity_uri \
                 FROM event_participations p \
                 JOIN statuses s ON s.id = p.status_id \
                 JOIN accounts a ON a.id = p.account_id \
                 WHERE s.uri = 'https://remote.test/events/100' \
                   AND a.actor_id = $1",
            )
            .bind(actor)
            .fetch_optional(&db)
            .await
            .unwrap()
        }
    };

    // Organizer invite → `invited` row with protocol + invite URI.
    let inv = invite(
        1,
        "https://remote.test/@org",
        json!("https://remote.test/events/100"),
        vec![alice_actor.clone()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    let row = participation().await.expect("invited row written");
    assert_eq!(row.0, "invited");
    assert_eq!(row.1, "invite_response");
    assert_eq!(row.2, "https://remote.test/ap/activities/inv1");

    // A non-organizer's invite is consumed but writes nothing.
    let inv = invite(
        2,
        "https://remote.test/@mallory",
        json!("https://remote.test/events/100"),
        vec![alice_actor.clone()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    let row = participation().await.unwrap();
    assert_eq!(
        row.2, "https://remote.test/ap/activities/inv1",
        "invite stamp unchanged"
    );

    // Remote-only addressee on a materialized event → handled, but
    // `write_invited_rows` filters to local accounts so nothing is
    // written.
    let inv = invite(
        3,
        "https://remote.test/@org",
        json!("https://remote.test/events/100"),
        vec!["https://remote.test/@bob".into()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    let bob_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM event_participations p \
         JOIN accounts a ON a.id = p.account_id \
         WHERE a.actor_id = 'https://remote.test/@bob'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(bob_rows, 0);

    // Non-Event object → falls through.
    let inv = invite(
        4,
        "https://remote.test/@org",
        json!({"id": "https://remote.test/notes/9", "type": "Note"}),
        vec![alice_actor.clone()],
    );
    assert!(
        !yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );

    // A decided state is never clobbered by a later invite.
    sqlx::query(
        "UPDATE event_participations SET state = 'accepted' \
         WHERE status_id = (SELECT id FROM statuses WHERE uri = 'https://remote.test/events/100')",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    let inv = invite(
        5,
        "https://remote.test/@org",
        json!("https://remote.test/events/100"),
        vec![alice_actor.clone()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    let row = participation().await.unwrap();
    assert_eq!(row.0, "accepted", "decided state preserved");
    assert_eq!(
        row.2, "https://remote.test/ap/activities/inv1",
        "invite stamp untouched"
    );
}

/// A bare-URI `Invite{Event}` that arrives before the event federates
/// parks its addressing in `event_pending_invites`; the event's later
/// ingest drains it into `invited` rows — and re-checks the sender's
/// organizer authorization at drain time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_bare_uri_invite_parks_and_drains() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let _token = register(&env.app, "bob", "bob@x.com").await;
    let bob_actor = format!("{BASE}/ap/users/bob");

    let invite = |n: usize, actor: &str, uri: &str, to: Vec<String>| pubby_sdk::Activity {
        id: format!("https://remote.test/ap/activities/pinv{n}"),
        activity_type: "Invite".into(),
        actor: actor.into(),
        object: json!(uri),
        to,
        cc: vec![],
        published: None,
        signature: None,
    };
    let event = |id: &str, uri: &str| {
        json!({
            "id": uri,
            "type": "Event",
            "name": "Parked invite",
            "attributedTo": id,
            "startTime": "2026-04-01T10:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };
    let create = |n: usize, actor: &str, object: Value| pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/ev{n}"),
        activity_type: "Create".into(),
        actor: actor.into(),
        object,
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let participation = |uri: &str| {
        let db = env.state.db.clone();
        let uri = uri.to_string();
        let actor = bob_actor.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS( \
                     SELECT 1 FROM event_participations p \
                     JOIN statuses s ON s.id = p.status_id \
                     JOIN accounts a ON a.id = p.account_id \
                     WHERE s.uri = $1 AND a.actor_id = $2)",
            )
            .bind(uri)
            .bind(actor)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let parked = |uri: &str| {
        let db = env.state.db.clone();
        let uri = uri.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM event_pending_invites WHERE event_uri = $1",
            )
            .bind(uri)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };

    // Invite for an event that hasn't federated → parked, fetch
    // enqueued, handled.
    let inv = invite(
        1,
        "https://remote.test/@org",
        "https://remote.test/events/200",
        vec![bob_actor.clone()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    assert_eq!(parked("https://remote.test/events/200").await, 1);
    assert!(!participation("https://remote.test/events/200").await);

    // A non-organizer's parked invite is dropped at drain time.
    let inv = invite(
        2,
        "https://remote.test/@mallory",
        "https://remote.test/events/300",
        vec![bob_actor.clone()],
    );
    assert!(
        yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap()
    );
    assert_eq!(parked("https://remote.test/events/300").await, 1);

    // The events materialize — the organizer's invite drains into a
    // row, the stranger's is dropped.
    let c = create(
        200,
        "https://remote.test/@org",
        event("https://remote.test/@org", "https://remote.test/events/200"),
    );
    assert!(remote::handle(&core, &c, &c.to_json()).await);
    assert!(participation("https://remote.test/events/200").await);
    assert_eq!(parked("https://remote.test/events/200").await, 0);

    let c = create(
        300,
        "https://remote.test/@org",
        event("https://remote.test/@org", "https://remote.test/events/300"),
    );
    assert!(remote::handle(&core, &c, &c.to_json()).await);
    assert!(!participation("https://remote.test/events/300").await);
    assert_eq!(parked("https://remote.test/events/300").await, 0);
}

/// `sync_event` on Update: a moved `startTime` updates the row and
/// re-arms participant reminders (`reminded_at` reset).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_update_moves_start() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let _token = register(&env.app, "carol", "carol@x.com").await;
    let carol_actor = format!("{BASE}/ap/users/carol");

    let event_obj = |start: &str| {
        json!({
            "id": "https://remote.test/events/400",
            "type": "Event",
            "name": "Movable",
            "attributedTo": "https://remote.test/@org",
            "startTime": start,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };
    let activity = |n: usize, kind: &str, object: Value| pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/mv{n}"),
        activity_type: kind.into(),
        actor: "https://remote.test/@org".into(),
        object,
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let c = activity(1, "Create", event_obj("2026-05-01T10:00:00Z"));
    assert!(remote::handle(&core, &c, &c.to_json()).await);

    // A participant row with a fired reminder.
    sqlx::query(
        "INSERT INTO event_participations (status_id, account_id, role, state, protocol, reminded_at) \
         SELECT s.id, a.id, 'participant', 'accepted', 'join', now() \
         FROM statuses s, accounts a \
         WHERE s.uri = 'https://remote.test/events/400' AND a.actor_id = $1",
    )
    .bind(&carol_actor)
    .execute(&env.state.db)
    .await
    .unwrap();

    let u = activity(2, "Update", event_obj("2026-05-02T10:00:00Z"));
    assert!(remote::handle(&core, &u, &u.to_json()).await);

    let (start, reminded): (
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT e.start_time, p.reminded_at \
             FROM events e \
             JOIN statuses s ON s.id = e.status_id \
             JOIN event_participations p ON p.status_id = s.id \
             JOIN accounts a ON a.id = p.account_id \
             WHERE s.uri = 'https://remote.test/events/400' AND a.actor_id = $1",
    )
    .bind(&carol_actor)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(start.to_rfc3339(), "2026-05-02T10:00:00+00:00");
    assert!(reminded.is_none(), "moved start re-arms the reminder");
}

/// Event Update ownership: the incoming object's self-asserted
/// `actor`/`organizers` list is not trusted on Update — a same-host
/// stranger cannot rewrite a Group's event, and ownership never moves.
/// A *stored* organizer (disclosed at Create) may still sign Updates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_update_ownership() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();

    // Group event with a disclosed organizer — the stored organizer
    // row is what later Updates are authorized against.
    let event_obj = |name: &str, actor: &str, attributed: Value| {
        json!({
            "id": "https://remote.test/events/600",
            "type": "Event",
            "name": name,
            "attributedTo": attributed,
            "actor": actor,
            "organizers": ["https://remote.test/@alice"],
            "startTime": "2026-06-01T10:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };
    let activity = |n: usize, kind: &str, actor: &str, object: Value| pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/uo{n}"),
        activity_type: kind.into(),
        actor: actor.into(),
        object,
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let title = || {
        let db = env.state.db.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT s.title FROM statuses s WHERE s.uri = 'https://remote.test/events/600'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
            .unwrap()
        }
    };

    let c = activity(
        1,
        "Create",
        "https://remote.test/@alice",
        event_obj(
            "Original",
            "https://remote.test/@alice",
            json!({"id": "https://remote.test/@club", "type": "Group"}),
        ),
    );
    assert!(remote::handle(&core, &c, &c.to_json()).await);
    assert_eq!(title().await, "Original");

    // A same-host stranger lists itself as `actor` — the Create gate
    // would accept this shape, but Updates require a stored organizer.
    let u = activity(
        2,
        "Update",
        "https://remote.test/@mallory",
        event_obj(
            "Hijacked",
            "https://remote.test/@mallory",
            json!({"id": "https://remote.test/@club", "type": "Group"}),
        ),
    );
    assert!(remote::handle(&core, &u, &u.to_json()).await);
    assert_eq!(title().await, "Original", "self-asserted signer rejected");

    // Ownership move: the stranger attributes the event to itself.
    let u = activity(
        3,
        "Update",
        "https://remote.test/@mallory",
        event_obj(
            "Hijacked",
            "https://remote.test/@mallory",
            json!("https://remote.test/@mallory"),
        ),
    );
    assert!(remote::handle(&core, &u, &u.to_json()).await);
    assert_eq!(title().await, "Original", "attributedTo move rejected");
    let owner: String = sqlx::query_scalar(
        "SELECT a.actor_id FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE s.uri = 'https://remote.test/events/600'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(owner, "https://remote.test/@club");

    // The disclosed organizer (a stored `event_organizers` row) may
    // sign the Update.
    let u = activity(
        4,
        "Update",
        "https://remote.test/@alice",
        event_obj(
            "Renamed",
            "https://remote.test/@alice",
            json!({"id": "https://remote.test/@club", "type": "Group"}),
        ),
    );
    assert!(remote::handle(&core, &u, &u.to_json()).await);
    assert_eq!(title().await, "Renamed", "stored organizer accepted");
}

/// `event_pending_invites` abuse bounds: parking stops at a per-sender
/// cap, and the retention sweep (`prune_pending_invites`) removes
/// expired rows while leaving fresh ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_pending_invite_cap_and_prune() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let _token = register(&env.app, "erin", "erin@x.com").await;
    let erin_actor = format!("{BASE}/ap/users/erin");

    // Fill the per-sender cap straight from SQL.
    sqlx::query(
        "INSERT INTO event_pending_invites \
             (event_uri, invite_activity_uri, actor, addressee) \
         SELECT 'https://remote.test/events/fill' || g, \
                'https://remote.test/ap/activities/finv' || g, \
                'https://remote.test/@spammer', $1 \
         FROM generate_series(1, 256) g",
    )
    .bind(&erin_actor)
    .execute(&env.state.db)
    .await
    .unwrap();

    let spammer_count = || {
        let db = env.state.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM event_pending_invites \
                 WHERE actor = 'https://remote.test/@spammer'",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(spammer_count().await, 256);

    // One more invite from the capped sender is dropped whole.
    let inv = pubby_sdk::Activity {
        id: "https://remote.test/ap/activities/finv999".into(),
        activity_type: "Invite".into(),
        actor: "https://remote.test/@spammer".into(),
        object: json!("https://remote.test/events/999"),
        to: vec![erin_actor.clone()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(
        !yonder::events::remote::handle_invite(&core, &inv, &inv.to_json())
            .await
            .unwrap(),
        "capped sender's invite falls through"
    );
    assert_eq!(spammer_count().await, 256, "cap holds");

    // Retention: backdate the spammer's rows, keep a fresh one for
    // another sender.
    sqlx::query(
        "UPDATE event_pending_invites SET created_at = now() - interval '30 days' \
         WHERE actor = 'https://remote.test/@spammer'",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO event_pending_invites \
             (event_uri, invite_activity_uri, actor, addressee) \
         VALUES ('https://remote.test/events/fresh', \
                 'https://remote.test/ap/activities/finv-fresh', \
                 'https://remote.test/@other', $1)",
    )
    .bind(&erin_actor)
    .execute(&env.state.db)
    .await
    .unwrap();

    let deleted = yonder::events::remote::prune_pending_invites(&env.state.db, 7)
        .await
        .unwrap();
    assert_eq!(deleted, 256);
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM event_pending_invites")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(total, 1, "fresh row survives the sweep");
}

/// The read path: a materialized event exposes `yonder.event`/
/// `pleroma.event` with locations, links and banner, and the
/// Mastodon-API `content` carries the When/Where preamble — which is
/// suppressed for event-aware clients (pl-fe) but shown to everyone
/// else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_entity_output() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let _token = register(&env.app, "frank", "frank@x.com").await;

    // Physical place + a Link attachment (not promoted to a virtual
    // location because a physical place exists) + a banner image.
    let create = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/ev500"),
        activity_type: "Create".into(),
        actor: "https://remote.test/@org".into(),
        object: json!({
            "id": "https://remote.test/events/500",
            "type": "Event",
            "name": "Entity test",
            "attributedTo": "https://remote.test/@org",
            "content": "<p>About the event</p>",
            "startTime": "2026-07-01T10:00:00Z",
            "location": {"type": "Place", "name": "Grand Hall"},
            "attachment": [
                {"type": "Link", "href": "https://remote.test/register", "name": "Register"},
                {"type": "Document", "mediaType": "image/jpeg",
                 "url": "https://remote.test/media/banner.jpg", "name": "Banner"}
            ],
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &create, &create.to_json()).await);

    let status_id: i64 =
        sqlx::query_scalar("SELECT id FROM statuses WHERE uri = 'https://remote.test/events/500'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();

    // Anonymous (non-event-aware) reader: full block + preamble.
    let (status, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ev = &body["yonder"]["event"];
    assert_eq!(ev["name"].as_str(), Some("Entity test"), "{body}");
    assert_eq!(ev["locations"][0]["name"].as_str(), Some("Grand Hall"));
    assert_eq!(
        ev["links"][0]["href"].as_str(),
        Some("https://remote.test/register"),
        "Link attachment persisted"
    );
    assert!(
        ev["banner"].is_object(),
        "banner resolved to a media entity: {ev}"
    );
    assert_eq!(
        body["pleroma"]["event"]["name"].as_str(),
        Some("Entity test")
    );
    let content = body["content"].as_str().unwrap();
    assert!(content.contains("When:"), "preamble present: {content}");
    assert!(content.contains("About the event"));

    // An event-aware client (pl-fe) gets no preamble — it renders the
    // structured block itself.
    let user_id: i64 = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE a.username = 'frank'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let app_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_applications (name, client_id, client_secret_hash, confidential) \
         VALUES ('pl-fe', 'plfe-client', 'x', false) RETURNING id",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let plfe_token = "plfe-test-token-0123456789abcdef";
    sqlx::query(
        "INSERT INTO oauth_access_tokens (application_id, user_id, token_hash, scopes, name) \
         VALUES ($1, $2, $3, 'read', 'pl-fe tok')",
    )
    .bind(app_id)
    .bind(user_id)
    .bind(yonder::http::middleware::auth::token_hash(plfe_token))
    .execute(&env.state.db)
    .await
    .unwrap();
    let (status, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        Some(plfe_token),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let content = body["content"].as_str().unwrap();
    assert!(
        !content.contains("When:"),
        "preamble suppressed for event-aware clients: {content}"
    );
    assert!(content.contains("About the event"));
    assert_eq!(
        body["yonder"]["event"]["name"].as_str(),
        Some("Entity test")
    );
}

/// A bare remote stub still gets one `RemoteActorFetch`, but once
/// `accounts.actor_data` holds a real document the stub is already
/// enriched — later activities must not re-enqueue the job (regression:
/// active remote actors flooded `yonder:queue:default` with redundant
/// fetches).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enriched_remote_actor_does_not_refetch() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let actor = "https://remote.test/users/eve";
    let note = |n: i64| pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/eve-{n}"),
        activity_type: "Create".into(),
        actor: actor.into(),
        object: json!({
            "id": format!("https://remote.test/objects/eve-{n}"),
            "type": "Note",
            "attributedTo": actor,
            "content": format!("<p>note {n}</p>"),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };

    let create = note(1);
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert_eq!(
        jobs.iter()
            .filter(|j| j.kind == JobKind::RemoteActorFetch)
            .count(),
        1,
        "{jobs:?}"
    );

    // Simulate the enrichment job having landed — a full actor
    // document, not a partial follower-record stub.
    sqlx::query(
        "UPDATE accounts SET actor_data = jsonb_build_object( \
             'id', $1::text, 'type', 'Person', 'preferredUsername', 'eve', \
             'inbox', $1::text || '/inbox') WHERE actor_id = $1",
    )
    .bind(actor)
    .execute(&env.state.db)
    .await
    .unwrap();

    let create = note(2);
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert!(
        !jobs.iter().any(|j| j.kind == JobKind::RemoteActorFetch),
        "enriched actor re-enqueued RemoteActorFetch: {jobs:?}"
    );

    // A partial doc (`id`/`type` only, as follower records carry) is
    // not enrichment — the fetch must still be enqueued.
    sqlx::query("UPDATE accounts SET actor_data = '{\"id\":\"x\",\"type\":\"Person\"}'::jsonb WHERE actor_id = $1")
        .bind(actor)
        .execute(&env.state.db)
        .await
        .unwrap();
    let create = note(3);
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert_eq!(
        jobs.iter()
            .filter(|j| j.kind == JobKind::RemoteActorFetch)
            .count(),
        1,
        "partial actor_data suppressed RemoteActorFetch: {jobs:?}"
    );
}

/// WriteFreely `Article`s put the headline in `name`, an HTML teaser
/// (header image + `[…]`) in `summary`, the body in `content` and the
/// permalink in `url`. Mastodon converts this to title + teaser +
/// permalink text with *no* CW — and so do we, keeping the body.
/// `summary`'s markup must never reach `spoiler_text` verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_article_converted_text() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/wf1"),
        activity_type: "Create".into(),
        actor: "https://blog.remote/users/eve".into(),
        object: json!({
            "id": "https://blog.remote/api/posts/kite1",
            "type": "Article",
            "attributedTo": "https://blog.remote/users/eve",
            "name": "When a Child’s Kite Becomes a Military Threat",
            "summary": "<p><img src=\"https://i.postimg.cc/x/IMG.jpg\"> […]</p>",
            "url": "https://blog.remote/eve/when-a-childs-kite",
            "content": "<p><img src=\"https://i.postimg.cc/x/IMG.jpg\"></p>\n\n<p>A child flies a kite.</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-08-24T09:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let handled = remote::handle(&core, &activity, &activity.to_json()).await;
    assert!(handled);

    let row: (Option<String>, String, String, String) = sqlx::query_as(
        "SELECT spoiler_text, content_html, content, url \
         FROM statuses WHERE uri = 'https://blog.remote/api/posts/kite1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (spoiler, html, text, url) = row;
    // Converted objects carry no CW — the teaser lives in the text.
    assert!(spoiler.is_none(), "article summary must not become a CW");
    assert_eq!(url, "https://blog.remote/eve/when-a-childs-kite");
    // Headline, stripped teaser, sanitized body, permalink footer.
    assert!(
        html.contains("<strong>When a Child’s Kite"),
        "headline: {html}"
    );
    assert!(html.contains("<p>[…]</p>"), "teaser in text: {html}");
    // Converted types keep body `<img>`s inline unconditionally —
    // Friendica articles carry images only in `content`. The teaser
    // img stays stripped (its `[…]` marker is the only remnant).
    assert_eq!(html.matches("<img").count(), 1, "{html}");
    assert!(
        html.contains("data-yonder-inline-image"),
        "body img kept inline: {html}"
    );
    assert!(
        html.contains("src=\"https://i.postimg.cc/x/IMG.jpg\""),
        "{html}"
    );
    assert!(html.contains("A child flies a kite."));
    assert!(
        html.contains("href=\"https://blog.remote/eve/when-a-childs-kite\""),
        "permalink footer: {html}"
    );
    // The permalink lands in searchable text — that's what lets the
    // link-preview job pick it up for the article card.
    assert!(text.contains("https://blog.remote/eve/when-a-childs-kite"));

    // A Note's HTML summary still becomes a CW — stripped, never raw.
    let note = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/wf2"),
        activity_type: "Create".into(),
        actor: "https://blog.remote/users/eve".into(),
        object: json!({
            "id": "https://blog.remote/objects/n1",
            "type": "Note",
            "attributedTo": "https://blog.remote/users/eve",
            "summary": "<p><b>spoilers</b> ahead</p>",
            "content": "<p>the twist</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-08-24T10:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    remote::handle(&core, &note, &note.to_json()).await;
    let cw: String = sqlx::query_scalar(
        "SELECT spoiler_text FROM statuses WHERE uri = 'https://blog.remote/objects/n1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(cw, "spoilers ahead");
}

/// FEP-b2b8 local articles: `post_type=article` + `title`/`summary`/
/// `cover_media_id` persist, surface in `yonder.article`, emit an
/// `Article` AP object (`name`/`summary`/`image`/`preview`/`source`),
/// round-trip through edits, and honor `default_post_type`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_article_post() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ann", "ann@example.com").await;

    // Validation: article needs a title; notes reject article fields;
    // unknown post types 422.
    for body in [
        json!({"status": "x", "post_type": "article"}),
        json!({"status": "x", "post_type": "article", "title": "  "}),
        json!({"status": "x", "title": "t"}),
        json!({"status": "x", "post_type": "blog"}),
    ] {
        let (status, resp) = request(
            &env.app,
            "POST",
            "/api/v1/statuses",
            Some(&token),
            Some(body),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{resp}");
    }

    // A cover must be an image attachment of the same post.
    let (mstatus, media) = multipart(&env.app, "/api/v1/media", &token, &png_bytes(), &[]).await;
    assert_eq!(mstatus, StatusCode::OK, "{media}");
    let media_id = media["id"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "body without the image",
            "post_type": "article",
            "title": "T",
            "cover_media_id": media_id,
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "Once upon a time there was a blog.",
            "post_type": "article",
            "title": "My First Post",
            "summary": "A short teaser",
            "spoiler_text": "CW",
            "media_ids": [media_id],
            "cover_media_id": media_id,
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap();
    assert_eq!(body["yonder"]["article"]["title"], "My First Post");
    assert_eq!(body["yonder"]["article"]["summary"], "A short teaser");
    assert_eq!(
        body["yonder"]["article"]["cover"]["id"],
        media["id"].as_str().unwrap()
    );
    assert!(body["yonder"]["article"]["cover"]["url"].is_string());

    // The AP object is an `Article` with the FEP-b2b8 properties.
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/ap/users/ann/statuses/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["type"], "Article");
    assert_eq!(doc["name"], "My First Post");
    assert_eq!(doc["attributedTo"], format!("{BASE}/ap/users/ann"));
    assert!(doc["published"].is_string());
    // summary = CW + lede as HTML paragraphs.
    let summary = doc["summary"].as_str().unwrap();
    assert!(summary.contains("<p>CW</p>"), "{summary}");
    assert!(summary.contains("<p>A short teaser</p>"), "{summary}");
    assert_eq!(doc["image"]["type"], "Image");
    assert!(doc["image"]["url"].as_str().unwrap().contains("/media/"));
    assert_eq!(doc["preview"]["type"], "Note");
    assert_eq!(doc["preview"]["name"], "My First Post");
    assert_eq!(
        doc["source"]["content"],
        "Once upon a time there was a blog."
    );
    assert_eq!(doc["generator"]["type"], "Application");

    // Edit → note: article metadata clears, the object becomes a Note.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "now a note", "post_type": "note"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["yonder"]["article"].is_null(), "{body}");
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/ap/users/ann/statuses/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["type"], "Note");
    assert!(doc.get("name").is_none());
    assert!(doc.get("preview").is_none());

    // `default_post_type` setting: PATCH, then an unspecified post
    // type resolves to article.
    let (status, body) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&token),
        Some(json!({"default_post_type": "article"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_post_type"], "article");
    assert_eq!(body["effective_post_type"], "article");
    let (status, body) = request(
        &env.app,
        "PATCH",
        "/api/v1/yonder/settings",
        Some(&token),
        Some(json!({"default_post_type": "blog"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "defaulted article", "title": "Second"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["yonder"]["article"]["title"], "Second");

    // An explicit note overrides the article default.
    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "still a note", "post_type": "note"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["yonder"]["article"].is_null(), "{body}");
}

/// Songhive/Funkwhale tracks arrive as `Create{Audio}` — the object
/// carries a `url` array (media + `text/html` track page) and an
/// `audio/*` Document attachment. The materialized status keeps the
/// HTML permalink as `url` and an `audio` media row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_audio_materializes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/audio-1"),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": "https://remote.test/objects/track-1",
            "type": "Audio",
            "attributedTo": [
                "https://remote.test/users/eve",
                "https://remote.test/artists/eve",
            ],
            "name": "eve - track one",
            "content": "<p>a song I composed</p>",
            "url": [
                {"type": "Link", "href": "https://remote.test/files/track-1.mp3",
                 "mediaType": "audio/mp3"},
                {"type": "Link", "href": "https://remote.test/tracks/track-1",
                 "mediaType": "text/html"},
            ],
            "attachment": [{
                "type": "Document",
                "mediaType": "audio/mp3",
                "url": "https://remote.test/files/track-1.mp3",
                "name": "track one",
            }],
            "image": {
                "type": "Image",
                "url": "https://remote.test/art/track-1.png",
            },
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let handled = remote::handle(&core, &activity, &activity.to_json()).await;
    assert!(handled, "Create{{Audio}} must be handled");

    let row: Option<(bool, String)> = sqlx::query_as(
        "SELECT local, url FROM statuses WHERE uri = 'https://remote.test/objects/track-1'",
    )
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    let (local, url) = row.expect("remote Audio materialized");
    assert!(!local);
    // The `text/html` Link wins — the media-file href is not a permalink.
    assert_eq!(url, "https://remote.test/tracks/track-1");

    let (kind, remote_url, content_type, preview): (String, String, String, String) =
        sqlx::query_as(
            "SELECT type, remote_url, content_type, preview_remote_url FROM media_attachments \
             WHERE status_id = (SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/track-1')",
        )
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(kind, "audio");
    assert_eq!(remote_url, "https://remote.test/files/track-1.mp3");
    assert_eq!(content_type, "audio/mp3");
    // The object's `image` became the attachment's poster art.
    assert_eq!(preview, "https://remote.test/art/track-1.png");

    // …and the entity surfaces it as `preview_url` (cover art for
    // players), not the mp3 itself.
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/track-1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let token = register(&env.app, "mia", "mia@example.com").await;
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
        &[],
    )
    .await;
    let att = &body["media_attachments"][0];
    assert_eq!(att["type"], "audio", "{body}");
    assert_eq!(att["preview_url"], "https://remote.test/art/track-1.png");
}

/// PeerTube `Video` objects carry no `attachment` — the playable
/// source lives in `url` Links and the poster in `icon`. Ingest
/// synthesizes a `video` media row from the best playable link
/// (`video/*` first, then HLS) and picks the largest thumbnail as
/// `preview_url` poster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_video_synthesizes_attachment() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/video-1"),
        activity_type: "Create".into(),
        actor: "https://remote.test/accounts/lunatics".into(),
        object: json!({
            "id": "https://remote.test/videos/watch/0200ef42",
            "type": "Video",
            "name": "Textless episode titles",
            "attributedTo": [
                {"type": "Person", "id": "https://remote.test/accounts/lunatics"},
                {"type": "Group", "id": "https://remote.test/video-channels/lunatics"},
            ],
            // PeerTube declares `mediaType: text/markdown` and sends
            // raw Markdown source — rendered, not sanitized as HTML.
            "mediaType": "text/markdown",
            "content": "Textless episode titles / end credits\r\n\r\nMore on the [Lunatics Log](https://lunaticsproject.org) #film",
            "url": [
                {"type": "Link", "href": "https://remote.test/w/1fmju4eq",
                 "mediaType": "text/html"},
                {"type": "Link", "href": "https://remote.test/videos/watch/0200ef42",
                 "mediaType": "text/html"},
                {"type": "Link",
                 "href": "https://remote.test/static/streaming-playlists/hls/0200ef42/master.m3u8",
                 "mediaType": "application/x-mpegURL"},
                {"type": "Link", "href": "https://remote.test/tracker/announce"},
            ],
            "icon": [
                {"type": "Image", "mediaType": "image/jpeg",
                 "url": "https://remote.test/thumb/small.jpg",
                 "width": 280, "height": 157},
                {"type": "Image", "mediaType": "image/jpeg",
                 "url": "https://remote.test/thumb/large.jpg",
                 "width": 1280, "height": 720},
            ],
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    let handled = remote::handle(&core, &activity, &activity.to_json()).await;
    assert!(handled, "Create{{Video}} must be handled");

    // The Markdown `content` rendered to real HTML — link anchors,
    // paragraph breaks — rather than staying literal markup.
    let content_html: String = sqlx::query_scalar(
        "SELECT content_html FROM statuses WHERE uri = 'https://remote.test/videos/watch/0200ef42'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        content_html.contains(r#"<a href="https://lunaticsproject.org""#),
        "markdown link not rendered: {content_html}"
    );
    assert!(!content_html.contains("[Lunatics Log]"), "{content_html}");
    let tag_synced: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM status_tags st JOIN tags t ON t.id = st.tag_id \
         WHERE st.status_id = (SELECT id FROM statuses WHERE uri = 'https://remote.test/videos/watch/0200ef42') \
           AND t.name = 'film')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(tag_synced, "content-discovered hashtag not synced");

    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT m.type, m.remote_url, m.content_type, m.preview_remote_url, m.description \
         FROM media_attachments m \
         WHERE m.status_id = (SELECT id FROM statuses WHERE uri = 'https://remote.test/videos/watch/0200ef42')",
    )
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    let (kind, remote_url, content_type, preview, description) =
        row.expect("url-Link attachment synthesized");
    assert_eq!(kind, "video");
    assert_eq!(
        remote_url,
        "https://remote.test/static/streaming-playlists/hls/0200ef42/master.m3u8"
    );
    assert_eq!(content_type, "application/x-mpegURL");
    // Largest `icon` rendition wins the poster slot.
    assert_eq!(preview, "https://remote.test/thumb/large.jpg");
    assert_eq!(description, "Textless episode titles");

    // The status keeps the `text/html` watch page as its permalink…
    let url: String = sqlx::query_scalar(
        "SELECT url FROM statuses WHERE uri = 'https://remote.test/videos/watch/0200ef42'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(url, "https://remote.test/w/1fmju4eq");

    // …and the entity emits the playable attachment with the poster.
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/videos/watch/0200ef42'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let token = register(&env.app, "ned", "ned@example.com").await;
    let (_, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        None,
        &[],
    )
    .await;
    let att = &body["media_attachments"][0];
    assert_eq!(att["type"], "video", "{body}");
    assert_eq!(att["preview_url"], "https://remote.test/thumb/large.jpg");
    assert_eq!(
        att["url"],
        "https://remote.test/static/streaming-playlists/hls/0200ef42/master.m3u8"
    );
}

/// Drain every pending entry from every job stream and return the
/// decoded envelopes — the test stands in for the worker. Remote work
/// is spread across dedicated lanes, and drained remote envelopes
/// release their dedupe claims like a real ack would.
async fn drain_default_queue(env: &Env) -> Vec<JobEnvelope> {
    let mut out = Vec::new();
    for queue_name in Queue::ALL {
        out.extend(drain_queue(env, queue_name.as_str()).await);
    }
    out
}

/// Same drain for any queue name — the preview sweep fans out on
/// `bulk`.
async fn drain_queue(env: &Env, queue: &str) -> Vec<JobEnvelope> {
    let key = env.state.redis.key(&format!("queue:{queue}"));
    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn.xrange(&key, "-", "+").await.unwrap();
    let mut out = Vec::new();
    for entry in reply.ids {
        if let Some(v) = entry.map.get("job") {
            let body: String = redis::from_redis_value(v).unwrap();
            let envelope = queue::decode(&body).unwrap();
            if let Some(dedupe) = &envelope.dedupe_key {
                queue::release_dedupe(&env.state.redis, dedupe, &envelope.id).await;
            }
            out.push(envelope);
        }
        let _: i64 = conn.xdel(&key, &[entry.id]).await.unwrap();
    }
    out
}

/// A remote `Create` for a Note — `object_extra` merges content,
/// attachments and the like into the baseline object.
fn preview_create(n: i64, object_extra: Value) -> pubby_sdk::Activity {
    let mut object = json!({
        "id": format!("https://remote.test/objects/prev-{n}"),
        "type": "Note",
        "attributedTo": "https://remote.test/users/eve",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "published": "2025-01-01T00:00:00Z",
    });
    object
        .as_object_mut()
        .unwrap()
        .extend(object_extra.as_object().unwrap().clone());
    pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/prev-{n}"),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object,
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    }
}

/// Remote materialization is the remote equivalent of local
/// create/edit — a link-carrying Note must enqueue `FetchLinkPreview`
/// or its `card` never reaches the timeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_note_link_preview_enqueue() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();

    // A remote Note carrying an external URL enqueues the preview job.
    let create = preview_create(
        1,
        json!({"content": "<p>see https://example.com/article for details</p>"}),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let jobs = drain_default_queue(&env).await;
    let previews: Vec<&JobEnvelope> = jobs
        .iter()
        .filter(|j| j.kind == JobKind::FetchLinkPreview)
        .collect();
    assert_eq!(previews.len(), 1, "{jobs:?}");
    assert_eq!(
        previews[0].payload["status_id"].as_i64().unwrap(),
        status_id
    );

    // A redelivered Create changed nothing — no second enqueue.
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert!(
        !jobs.iter().any(|j| j.kind == JobKind::FetchLinkPreview),
        "{jobs:?}"
    );

    // An accepted Update re-runs the fetch (content may carry a new
    // URL) — `updated` must postdate the stored row's timestamp.
    let update = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/prev-1u"),
        activity_type: "Update".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": "https://remote.test/objects/prev-1",
            "type": "Note",
            "attributedTo": "https://remote.test/users/eve",
            "content": "<p>see https://example.com/article now</p>",
            "updated": "2100-01-01T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &update, &update.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert_eq!(
        jobs.iter()
            .filter(|j| j.kind == JobKind::FetchLinkPreview)
            .count(),
        1,
        "{jobs:?}"
    );

    // Media-attached remote statuses never enqueue (card suppressed).
    let create = preview_create(
        2,
        json!({
            "content": "<p>see https://example.com/pic</p>",
            "attachment": [{
                "type": "Document",
                "url": "https://remote.test/media/1.png",
                "mediaType": "image/png",
            }],
        }),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert!(
        !jobs.iter().any(|j| j.kind == JobKind::FetchLinkPreview),
        "{jobs:?}"
    );

    // URL-free remote statuses enqueue nothing either.
    let create = preview_create(3, json!({"content": "<p>no links here</p>"}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let jobs = drain_default_queue(&env).await;
    assert!(
        !jobs.iter().any(|j| j.kind == JobKind::FetchLinkPreview),
        "{jobs:?}"
    );

    // Mastodon's invisible-span link markup flattens to the href —
    // the URL must survive into `content` or the extractor finds only
    // a bare `https://` and no card is ever fetched.
    let create = preview_create(
        8,
        json!({
            "content": "<p>watch <a href=\"https://youtu.be/abc123\">\
                        <span class=\"invisible\">https://</span>\
                        <span>youtu.be/abc123</span></a></p>",
        }),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-8'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(content, "watch https://youtu.be/abc123");
    let jobs = drain_default_queue(&env).await;
    assert_eq!(
        jobs.iter()
            .filter(|j| j.kind == JobKind::FetchLinkPreview)
            .count(),
        1,
        "{jobs:?}"
    );
}

/// A stale `preview_cards` row for the status — what a re-run must
/// reconcile away.
async fn seed_card(db: &sqlx::PgPool, status_id: i64) {
    sqlx::query(
        "INSERT INTO preview_cards (status_id, url, title) \
         VALUES ($1, 'https://example.com/old', 'Old')",
    )
    .bind(status_id)
    .execute(db)
    .await
    .unwrap();
}

async fn card_count(db: &sqlx::PgPool, status_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM preview_cards WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Job re-runs converge `preview_cards` to the status' current
/// eligibility: media or a dropped URL clears the card, a superseded
/// first-URL is replaced — never left stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_preview_job_converges_cards() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let job = FetchLinkPreviewJob::new(env.state.ap.clone());
    let jctx = JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    };

    // A media-attached status that somehow holds a card loses it.
    let create = preview_create(
        4,
        json!({
            "content": "<p>see https://example.com/pic</p>",
            "attachment": [{
                "type": "Document",
                "url": "https://remote.test/media/2.png",
                "mediaType": "image/png",
            }],
        }),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let media_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-4'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    seed_card(&env.state.db, media_id).await;
    job.run(&jctx, &json!({"status_id": media_id}))
        .await
        .unwrap();
    assert_eq!(card_count(&env.state.db, media_id).await, 0);

    // A URL-less status likewise.
    let create = preview_create(5, json!({"content": "<p>no links here</p>"}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let bare_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-5'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    seed_card(&env.state.db, bare_id).await;
    job.run(&jctx, &json!({"status_id": bare_id}))
        .await
        .unwrap();
    assert_eq!(card_count(&env.state.db, bare_id).await, 0);

    // A card for a superseded first-URL is dropped. The fetch target
    // (127.0.0.1:9, discard port) refuses instantly — the reconcile
    // already ran before the fetch fails.
    let create = preview_create(6, json!({"content": "<p>see http://127.0.0.1:9/x</p>"}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let link_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-6'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    seed_card(&env.state.db, link_id).await;
    job.run(&jctx, &json!({"status_id": link_id}))
        .await
        .unwrap();
    let urls: Vec<String> =
        sqlx::query_scalar("SELECT url FROM preview_cards WHERE status_id = $1")
            .bind(link_id)
            .fetch_all(&env.state.db)
            .await
            .unwrap();
    assert!(
        !urls.iter().any(|u| u == "https://example.com/old"),
        "stale card must be reconciled: {urls:?}"
    );
}

/// Remote `content` flattened before anchors emitted their hrefs is
/// healed by the sweep: `content_html` kept the real URL, the
/// re-flatten restores it to `content`, and the status re-enqueues on
/// the bulk queue for a card — the reported "remote video link, no
/// embed" repair path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_preview_sweep_repairs_flattened_content() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();

    let create = preview_create(
        9,
        json!({
            "content": "<p>watch <a href=\"https://youtu.be/xyz789\">\
                        <span class=\"invisible\">https://</span>\
                        <span>youtu.be/xyz789</span></a></p>",
        }),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-9'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let _ = drain_default_queue(&env).await;

    // Pre-fix damage: the URL split at the scheme boundary and the
    // href never reached `content` — so no card was ever fetched.
    sqlx::query("UPDATE statuses SET content = 'watch https:// youtu.be/xyz789' WHERE id = $1")
        .bind(status_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let job = FetchLinkPreviewJob::new(env.state.ap.clone());
    job.run(&job_ctx(&env), &json!({})).await.unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(content, "watch https://youtu.be/xyz789");
    let jobs = drain_queue(&env, "bulk").await;
    assert!(
        jobs.iter().any(|j| j.kind == JobKind::FetchLinkPreview
            && j.payload["status_id"].as_i64() == Some(status_id)),
        "{jobs:?}"
    );
}

/// Mention and hashtag links never qualify as card candidates — a
/// mention at the top of a post must not render a profile-page card.
/// Mastodon skips `mention`/`hashtag` anchors; our equivalent check
/// is the status' own `status_mentions`/`status_tags` entities.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_preview_skips_mentions_and_tags() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let job = FetchLinkPreviewJob::new(env.state.ap.clone());
    let jctx = JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    };

    // A known remote account so its Mention tag lands in
    // `status_mentions`.
    sqlx::query(
        "INSERT INTO accounts (username, domain, actor_id, url) \
         VALUES ('alice', 'remote.test', 'https://remote.test/users/alice', \
                 'https://remote.test/@alice') \
         ON CONFLICT DO NOTHING",
    )
    .execute(&env.state.db)
    .await
    .unwrap();

    // `source` puts the link targets into `statuses.content` — the
    // mention profile URL and the tag page URL are the only links.
    let create = preview_create(
        7,
        json!({
            "content": "<p><a href=\"https://remote.test/@alice\">@alice</a> \
                        check <a href=\"https://remote.test/tags/rust\">#rust</a></p>",
            "source": {"content": "@alice@remote.test check https://remote.test/@alice \
                                   and https://remote.test/tags/rust"},
            "tag": [
                {"type": "Mention", "href": "https://remote.test/users/alice",
                 "name": "@alice@remote.test"},
                {"type": "Hashtag", "name": "#rust",
                 "href": "https://remote.test/tags/rust"},
            ],
        }),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/objects/prev-7'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Stale cards on the mention/tag targets are reconciled away —
    // before the exclusion the mention URL was the preview candidate.
    for url in [
        "https://remote.test/@alice",
        "https://remote.test/tags/rust",
    ] {
        sqlx::query("INSERT INTO preview_cards (status_id, url, title) VALUES ($1, $2, 'Stale')")
            .bind(status_id)
            .bind(url)
            .execute(&env.state.db)
            .await
            .unwrap();
    }
    job.run(&jctx, &json!({"status_id": status_id}))
        .await
        .unwrap();
    assert_eq!(card_count(&env.state.db, status_id).await, 0);
}

/// Video-platform pages (YouTube, Vimeo, …) declare an iframeable
/// player via `twitter:player` / `og:video`+`text/html` — the card
/// entity upgrades to `type: "video"` with the embed, so the client
/// renders click-to-play instead of a bare link. A `video/mp4`
/// `og:video` is a raw media file, not a player, and stays `link`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_preview_video_card_embeds() {
    let _permit = harness::acquire().await;

    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 98, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = format!("http://127.0.0.1:{port}");
    let watch_page = format!(
        "<html><head>\
         <meta property=\"og:title\" content=\"Cool video\">\
         <meta property=\"og:site_name\" content=\"VidTest\">\
         <meta property=\"og:image\" content=\"{origin}/thumb.png\">\
         <meta name=\"twitter:player\" content=\"{origin}/embed/abc\">\
         <meta name=\"twitter:player:width\" content=\"560\">\
         <meta name=\"twitter:player:height\" content=\"315\">\
         </head><body>watch</body></html>"
    );
    // `og:video` alone doubles for raw media — only a `text/html`
    // player is iframeable (Vimeo-shape, no twitter:player).
    let og_page = format!(
        "<html><head>\
         <meta property=\"og:title\" content=\"OG player\">\
         <meta property=\"og:video:secure_url\" content=\"{origin}/player/42\">\
         <meta property=\"og:video:type\" content=\"text/html\">\
         <meta property=\"og:video:width\" content=\"640\">\
         <meta property=\"og:video:height\" content=\"360\">\
         </head><body>og</body></html>"
    );
    let raw_page = format!(
        "<html><head>\
         <meta property=\"og:title\" content=\"Raw clip\">\
         <meta property=\"og:video\" content=\"{origin}/files/clip.mp4\">\
         <meta property=\"og:video:type\" content=\"video/mp4\">\
         </head><body>raw</body></html>"
    );
    // Second player page — the rescan part needs an URL the earlier
    // fetches haven't already populated the v2 metadata cache for.
    let watch2_page = format!(
        "<html><head>\
         <meta property=\"og:title\" content=\"Fresh video\">\
         <meta name=\"twitter:player\" content=\"{origin}/embed2/abc\">\
         </head><body>watch2</body></html>"
    );
    let fake = Router::new()
        .route(
            "/watch",
            axum::routing::get(move || {
                let page = watch_page.clone();
                async move { axum::response::Html(page) }
            }),
        )
        .route(
            "/og",
            axum::routing::get(move || {
                let page = og_page.clone();
                async move { axum::response::Html(page) }
            }),
        )
        .route(
            "/raw",
            axum::routing::get(move || {
                let page = raw_page.clone();
                async move { axum::response::Html(page) }
            }),
        )
        .route(
            "/watch2",
            axum::routing::get(move || {
                let page = watch2_page.clone();
                async move { axum::response::Html(page) }
            }),
        )
        .route(
            "/thumb.png",
            axum::routing::get(|| async { ([("content-type", "image/png")], PNG) }),
        );
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "vicky", "vicky@example.com").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let job = FetchLinkPreviewJob::new(env.state.ap.clone());
    let jctx = JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    };
    let db = env.state.db.clone();
    let status_id = move |n: i64| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM statuses WHERE uri = $1")
                .bind(format!("https://remote.test/objects/prev-{n}"))
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let app = env.app.clone();
    let card_of = move |id: i64| {
        let app = app.clone();
        let token = token.clone();
        async move {
            let (status, body) = request(
                &app,
                "GET",
                &format!("/api/v1/statuses/{id}"),
                Some(&token),
                None,
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["card"].clone()
        }
    };

    // twitter:player → video card, thumbnail downloaded, dims kept.
    let create = preview_create(10, json!({"content": format!("<p>see {origin}/watch</p>")}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let id = status_id(10).await;
    job.run(&jctx, &json!({"status_id": id})).await.unwrap();
    let (embed, w, h): (String, i32, i32) = sqlx::query_as(
        "SELECT embed_url, embed_width, embed_height FROM preview_cards WHERE status_id = $1",
    )
    .bind(id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(embed, format!("{origin}/embed/abc"));
    assert_eq!((w, h), (560, 315));
    let card = card_of(id).await;
    assert_eq!(card["type"], "video", "{card}");
    assert_eq!(card["embed_url"], format!("{origin}/embed/abc"));
    assert_eq!(card["width"], 560);
    assert_eq!(card["height"], 315);
    assert!(card["image"].is_string(), "{card}");
    assert_eq!(card["provider_name"], "VidTest");
    let html = card["html"].as_str().unwrap();
    assert!(
        html.contains(&format!("src=\"{origin}/embed/abc\"")),
        "synthesized iframe: {html}"
    );

    // og:video text/html without twitter:player → same upgrade.
    let create = preview_create(11, json!({"content": format!("<p>see {origin}/og</p>")}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let id = status_id(11).await;
    job.run(&jctx, &json!({"status_id": id})).await.unwrap();
    let card = card_of(id).await;
    assert_eq!(card["type"], "video", "{card}");
    assert_eq!(card["embed_url"], format!("{origin}/player/42"));
    assert_eq!(card["width"], 640);
    assert_eq!(card["height"], 360);

    // A raw-media og:video stays a plain link card — no player.
    let create = preview_create(12, json!({"content": format!("<p>see {origin}/raw</p>")}));
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let id = status_id(12).await;
    job.run(&jctx, &json!({"status_id": id})).await.unwrap();
    let card = card_of(id).await;
    assert_eq!(card["type"], "link", "{card}");
    assert_eq!(card["embed_url"], "");
    assert_eq!(card["html"], "");

    // ---- Existing posts: rescan sweep + cache-version bump ----
    // A card row written by pre-embed code (scan_version = 0, no
    // embed fields) is what an upgraded instance holds. The sweep
    // (`{}` payload — `yonder run-job fetch_link_preview`) marks it
    // scanned and fans out a per-status job on `bulk`.
    let create = preview_create(
        13,
        json!({"content": format!("<p>see {origin}/watch2</p>")}),
    );
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let id = status_id(13).await;
    sqlx::query("INSERT INTO preview_cards (status_id, url, title) VALUES ($1, $2, 'Old link')")
        .bind(id)
        .bind(format!("{origin}/watch2"))
        .execute(&env.state.db)
        .await
        .unwrap();
    // A poisoned pre-bump cache envelope — v2 must ignore it
    // entirely, or the rescan would re-stamp a link card.
    let v1_key = env.state.redis.key(&format!(
        "cache:preview:{}",
        &hex::encode(Sha256::digest(format!("{origin}/watch2").as_bytes()))[..32]
    ));
    {
        let mut conn = env.state.redis.conn().await.unwrap();
        let envelope = json!({
            "v": {
                "title": "STALE-CACHE", "description": "", "image": null,
                "author_name": "", "author_url": "",
                "provider_name": "", "provider_url": "",
            },
            "e": 4_000_000_000f64,
        });
        let _: () = conn.set(&v1_key, envelope.to_string()).await.unwrap();
    }

    // The creates above each left a `FetchLinkPreview` envelope on
    // `bulk` (their queue since the remote-lane split) — drain them so
    // the sweep assertion below sees only what the rescan fans out.
    let _ = drain_queue(&env, "bulk").await;
    job.run(&jctx, &json!({})).await.unwrap();
    let scan: i32 =
        sqlx::query_scalar("SELECT scan_version FROM preview_cards WHERE status_id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(
        scan,
        yonder::posts::preview::CARD_SCAN_VERSION,
        "sweep must mark the row scanned"
    );
    let jobs = drain_queue(&env, "bulk").await;
    let swept: Vec<_> = jobs
        .iter()
        .filter(|j| j.kind == JobKind::FetchLinkPreview)
        .collect();
    // Only the stale row fans out — the cards written above already
    // carry the current scan_version, so they are not re-enqueued.
    assert_eq!(swept.len(), 1, "{jobs:?}");
    assert_eq!(swept[0].payload["status_id"], id);

    // The fanned-out job refreshes the card — video embed in spite
    // of the poisoned v1 envelope.
    job.run(&jctx, &swept[0].payload).await.unwrap();
    let card = card_of(id).await;
    assert_eq!(card["type"], "video", "{card}");
    assert_eq!(card["title"], "Fresh video", "{card}");
    assert_eq!(card["embed_url"], format!("{origin}/embed2/abc"));

    // A second sweep finds nothing stale — the marker converged.
    job.run(&jctx, &json!({})).await.unwrap();
    assert!(
        drain_queue(&env, "bulk")
            .await
            .iter()
            .all(|j| j.kind != JobKind::FetchLinkPreview),
        "converged cards must not be re-enqueued"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_deletion_tombstones() {
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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

/// Remote-instance URL search dereferences our web permalinks with
/// `Accept: application/activity+json` — they must answer the AP
/// document, not the SPA shell (a 200 HTML reply is a silent miss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_permalinks_serve_ap_documents() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "olga", "olga@example.com").await;
    let (_, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "permalink me", "visibility": "public"})),
        &[],
    )
    .await;
    let id = body["id"].as_str().unwrap();

    let get = |path: String, accept: &'static str| {
        let app = env.app.clone();
        async move {
            app.oneshot(
                Request::get(path)
                    .header("accept", accept)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let json_doc = |resp: axum::response::Response| async move {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()
    };

    // The SPA route and the stored `statuses.url` shape both serve the
    // Note; the acct segment is decorative.
    let canonical = format!("{BASE}/ap/users/olga/statuses/{id}");
    for path in [
        format!("/statuses/{id}"),
        format!("/@olga/{id}"),
        format!("/@anybody/{id}"),
    ] {
        let resp = get(path.clone(), "application/activity+json").await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/activity+json"
        );
        let doc = json_doc(resp).await;
        assert_eq!(doc["type"], "Note", "{path}");
        assert_eq!(doc["id"], canonical, "{path}");
    }

    // `ld+json` with the ActivityStreams profile negotiates too.
    let resp = get(
        format!("/statuses/{id}"),
        r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_doc(resp).await["type"], "Note");

    // The profile permalink serves the actor document.
    let resp = get("/@olga".to_string(), "application/activity+json").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = json_doc(resp).await;
    assert_eq!(doc["type"], "Person");
    assert_eq!(doc["id"], format!("{BASE}/ap/users/olga"));

    // Misses are JSON 404s, never the SPA shell: unknown user, foreign
    // domain acct, non-numeric id, non-permalink path.
    for path in [
        "/@nobody".to_string(),
        "/@olga@elsewhere.example".to_string(),
        format!("/statuses/{id}9"),
        "/compose".to_string(),
    ] {
        let resp = get(path.clone(), "application/activity+json").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json_doc(resp).await["error"], "not found");
    }

    // Browser accepts still get the SPA shell (the test env has no
    // frontend/dist, so it's the embedded placeholder — text/html).
    let resp = get(format!("/statuses/{id}"), "text/html").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_cleanup_respects_scheduled_reservation() {
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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
    let _permit = harness::acquire().await;
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

// ---------------------------------------------------------------------------
// Thread context + reply counters
// ---------------------------------------------------------------------------

/// A remote `Create` for a Note replying to `parent_uri`; `n` keeps ids
/// unique per fabricated object.
fn remote_reply_create(n: i64, parent_uri: &str) -> pubby_sdk::Activity {
    pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/reply-{n}"),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": format!("https://remote.test/objects/reply-{n}"),
            "type": "Note",
            "attributedTo": "https://remote.test/users/eve",
            "content": format!("<p>remote reply {n}</p>"),
            "inReplyTo": parent_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_reply_counts_and_surfaces_in_context() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "nora", "nora@example.com").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();

    let (_, parent) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "root post"})),
        &[],
    )
    .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();
    let parent_uri = parent["uri"].as_str().unwrap().to_string();

    // Remote reply materializes against the known parent and counts.
    let create = remote_reply_create(1, &parent_uri);
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let count: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1::bigint")
        .bind(parent_id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // The context endpoint surfaces it as a descendant.
    let (status, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{parent_id}/context"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ctx}");
    let desc = ctx["descendants"].as_array().unwrap();
    assert_eq!(desc.len(), 1, "{ctx}");
    assert_eq!(
        desc[0]["uri"].as_str().unwrap(),
        "https://remote.test/objects/reply-1"
    );

    // A redelivered Create must not double-count — on the parent nor
    // on the remote author's `statuses_count`.
    let create = remote_reply_create(1, &parent_uri);
    assert!(remote::handle(&core, &create, &create.to_json()).await);
    let count: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1::bigint")
        .bind(parent_id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1, "redelivery must not double-count");
    let author_posts: i32 = sqlx::query_scalar(
        "SELECT statuses_count FROM accounts a \
         JOIN statuses s ON s.account_id = a.id WHERE s.uri = 'https://remote.test/objects/reply-1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        author_posts, 1,
        "remote ingest must bump the author's counter once"
    );

    // Remote Delete releases the counter and drops the descendant.
    let delete = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/del-reply-1"),
        activity_type: "Delete".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!("https://remote.test/objects/reply-1"),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &delete, &delete.to_json()).await);
    let count: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1::bigint")
        .bind(parent_id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "remote delete must release the counter");
    let author_posts: i32 = sqlx::query_scalar(
        "SELECT statuses_count FROM accounts WHERE actor_id = 'https://remote.test/users/eve'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        author_posts, 0,
        "remote delete must release the author's counter"
    );
    let (status, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{parent_id}/context"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ctx}");
    assert!(ctx["descendants"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_mention_is_linkified_and_recorded() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "olga", "olga@example.com").await;

    // Replying to own post — the compose prefill seeds "@olga ".
    let (_, parent) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "root"})),
        &[],
    )
    .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();
    let (_, reply) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "@olga talking to myself",
            "in_reply_to_id": parent_id,
        })),
        &[],
    )
    .await;
    let html = reply["content"].as_str().unwrap();
    assert!(
        html.contains(">@olga</a>") || html.contains(">olga</a>"),
        "self-mention must linkify: {html}"
    );
    let mentions = reply["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 1, "{mentions:?}");
    assert_eq!(mentions[0]["acct"].as_str().unwrap(), "olga");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_domain_mention_resolves_locally() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.instance.domain = "yonder.test".to_string();
    let Some(env) = test_env(config).await else {
        return;
    };
    register(&env.app, "olga", "olga@example.com").await;
    let pete = register(&env.app, "pete", "pete@example.com").await;

    // `@user@<this-instance>` names a local account — it resolves like
    // the bare `@user` form and must never fabricate a `domain=<self>`
    // remote row.
    let (_, status) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&pete),
        Some(json!({"status": "@olga@yonder.test hi"})),
        &[],
    )
    .await;
    let mentions = status["mentions"].as_array().unwrap();
    assert_eq!(mentions.len(), 1, "{mentions:?}");
    assert_eq!(mentions[0]["acct"].as_str().unwrap(), "olga");
    let html = status["content"].as_str().unwrap();
    assert!(html.contains("mention"), "{html}");
    let remote_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM accounts WHERE lower(domain) = 'yonder.test'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(remote_rows, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_visibility_and_ancestors() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "pete", "pete@example.com").await;
    let bob = register(&env.app, "quinn", "quinn@example.com").await;

    let (_, root) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "thread root"})),
        &[],
    )
    .await;
    let root_id = root["id"].as_str().unwrap().to_string();

    // Public reply (visible to all) + private reply (author/followers only).
    let (_, pub_reply) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "public reply", "in_reply_to_id": root_id})),
        &[],
    )
    .await;
    let pub_reply_id = pub_reply["id"].as_str().unwrap().to_string();
    let (_, priv_reply) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({
            "status": "private reply",
            "visibility": "private",
            "in_reply_to_id": root_id,
        })),
        &[],
    )
    .await;
    let priv_reply_id = priv_reply["id"].as_str().unwrap().to_string();
    // Nested reply under the public one — exercises the recursive walk.
    let (_, nested) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "nested", "in_reply_to_id": pub_reply_id})),
        &[],
    )
    .await;
    let nested_id = nested["id"].as_str().unwrap().to_string();

    // Anonymous context: public descendants only, ancestors of the reply.
    let (status, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{root_id}/context"),
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ctx}");
    let ids: Vec<&str> = ctx["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&pub_reply_id.as_str()), "{ids:?}");
    assert!(ids.contains(&nested_id.as_str()), "{ids:?}");
    assert!(
        !ids.contains(&priv_reply_id.as_str()),
        "private reply must not leak to anonymous: {ids:?}"
    );

    // The author sees everything, ancestors walk root→parent.
    let (status, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{nested_id}/context"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ctx}");
    let anc: Vec<&str> = ctx["ancestors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(anc, [root_id.as_str(), pub_reply_id.as_str()]);

    // A non-follower still cannot see the private reply.
    let (status, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{root_id}/context"),
        Some(&bob),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ctx}");
    let ids: Vec<&str> = ctx["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(!ids.contains(&priv_reply_id.as_str()), "{ids:?}");

    // Deleting the public reply releases the counter; the nested reply
    // under it is pruned from the context too.
    let (status, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/statuses/{pub_reply_id}"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, shown) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{root_id}"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert_eq!(
        shown["replies_count"].as_i64().unwrap(),
        1,
        "only the private reply remains: {shown}"
    );
    let (_, ctx) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{root_id}/context"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    let ids: Vec<&str> = ctx["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids, [priv_reply_id.as_str()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_to_direct_must_be_direct() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;

    let (status, dm) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "@bob secret", "visibility": "direct"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{dm}");
    let dm_id = dm["id"].as_str().unwrap();

    // An explicit wider audience is a leak — 422.
    for vis in ["public", "unlisted", "private"] {
        let (status, body) = request(
            &env.app,
            "POST",
            "/api/v1/statuses",
            Some(&bob),
            Some(json!({
                "status": "@alice reply", "in_reply_to_id": dm_id, "visibility": vis,
            })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{vis}: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("a reply to a direct status must be direct"),
            "{body}"
        );
    }

    // Omitted visibility inherits the conversation's scope.
    let (status, reply) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&bob),
        Some(json!({"status": "@alice reply", "in_reply_to_id": dm_id})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["visibility"], "direct");
    let reply_id = reply["id"].as_str().unwrap();

    // An edit can't flip a DM reply out of the conversation either —
    // the stored parent is the effective one when the body omits it.
    let (status, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{reply_id}"),
        Some(&bob),
        Some(json!({"status": "@alice edited", "visibility": "public"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Editing while staying direct is fine.
    let (status, edited) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{reply_id}"),
        Some(&bob),
        Some(json!({"status": "@alice edited", "visibility": "direct"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    assert_eq!(edited["visibility"], "direct");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mentioned_nonfollower_can_reply_to_followers_only() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let carol = register(&env.app, "carol", "carol@x.com").await;

    // A followers-only post mentioning carol — she doesn't follow
    // alice, but a mention grants audience (Mastodon's `permitted?`):
    // she can see it and reply to a thread addressed to her.
    let (status, post) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "@carol psst", "visibility": "private"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{post}");
    let post_id = post["id"].as_str().unwrap();

    let (status, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{post_id}"),
        Some(&carol),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, reply) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&carol),
        Some(json!({
            "status": "@alice hi back", "in_reply_to_id": post_id,
            "visibility": "private",
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

/// `POST /statuses/{id}/translate` — LibreTranslate backend against a
/// loopback fake; the wire is the Mastodon Translation entity and the
/// second call lands on the Redis cache (the fake counts hits).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translate_status_via_libretranslate() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _permit = harness::acquire().await;

    // Fake LibreTranslate: echoes `[target] q` plus a detected
    // language, counting calls so the cache assertion is real.
    let hits = Arc::new(AtomicUsize::new(0));
    let fake = {
        let hits = hits.clone();
        Router::new().route(
            "/translate",
            axum::routing::post(move |body: axum::Json<Value>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "translatedText": format!(
                            "[{}] {}",
                            body["target"].as_str().unwrap_or_default(),
                            body["q"].as_str().unwrap_or_default(),
                        ),
                        "detectedLanguage": {"language": "de", "confidence": 99},
                    }))
                }
            }),
        )
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

    let mut config = Config::default();
    config.content.translation_backend = yonder::config::TranslationBackend::LibreTranslate;
    config.content.libretranslate_url = Some(format!("http://127.0.0.1:{port}"));
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;
    let carol = register(&env.app, "carol", "carol@x.com").await;

    // The instance entities advertise the feature once configured.
    let (status, inst) = request(&env.app, "GET", "/api/v1/instance", None, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inst["configuration"]["translation"]["enabled"], true);
    let (status, inst2) = request(&env.app, "GET", "/api/v2/instance", None, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inst2["configuration"]["translation"]["enabled"], true);

    let (status, post) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "hallo welt", "language": "de"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{post}");
    let id = post["id"].as_str().unwrap();
    let url = format!("/api/v1/statuses/{id}/translate");

    // Anonymous → 401.
    let (status, _) = request(&env.app, "POST", &url, None, None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, tr) = request(
        &env.app,
        "POST",
        &url,
        Some(&alice),
        Some(json!({"lang": "es"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tr}");
    assert_eq!(tr["provider"], "LibreTranslate");
    assert_eq!(tr["detected_source_language"], "de");
    let content = tr["content"].as_str().unwrap();
    assert!(content.starts_with("[es] "), "{content}");
    assert!(content.contains("hallo welt"), "{content}");
    assert_eq!(tr["media_attachments"], json!([]));
    assert_eq!(tr["poll"], Value::Null);
    assert_eq!(tr["spoiler_text"], "");

    // Second identical call — the Redis cache serves it, the fake
    // stays at one hit.
    let (status, _) = request(
        &env.app,
        "POST",
        &url,
        Some(&alice),
        Some(json!({"lang": "es"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "cached translation must not re-hit the backend"
    );

    // `lang` as a query param works too (Mastodon clients differ).
    let (status, tr) = request(
        &env.app,
        "POST",
        &format!("{url}?lang=fr"),
        Some(&alice),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tr}");
    assert!(tr["content"].as_str().unwrap().starts_with("[fr]"));

    // Omitted lang falls back to the viewer's locale ("en" here).
    let (status, tr) = request(&env.app, "POST", &url, Some(&alice), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{tr}");
    assert!(tr["content"].as_str().unwrap().starts_with("[en]"), "{tr}");

    // A malformed lang is a client bug — 422.
    let (status, body) = request(
        &env.app,
        "POST",
        &url,
        Some(&alice),
        Some(json!({"lang": "../../etc"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Gated content stays behind `can_see`, but a viewer with audience
    // can translate it: bob sees the followers-only mention and the
    // DM, so both go through the admin-configured backend.
    for (label, vis) in [("followers-only", "private"), ("direct", "direct")] {
        let (status, gated) = request(
            &env.app,
            "POST",
            "/api/v1/statuses",
            Some(&alice),
            Some(json!({"status": format!("@bob {label}"), "visibility": vis})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{gated}");
        let gid = gated["id"].as_str().unwrap();
        let (status, tr) = request(
            &env.app,
            "POST",
            &format!("/api/v1/statuses/{gid}/translate"),
            Some(&bob),
            Some(json!({"lang": "en"})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: {tr}");
        assert_eq!(tr["provider"], "LibreTranslate", "{label}: {tr}");
    }

    // …and a viewer who can't see the post at all gets a bare 404.
    let (status, gated) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "followers only", "visibility": "private"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{gated}");
    let gid = gated["id"].as_str().unwrap();
    let (status, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{gid}/translate"),
        Some(&carol),
        Some(json!({"lang": "en"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Without a usable backend (`libretranslate` set but no URL) the
/// instance reports `translation.enabled = false` and the endpoint
/// rejects with 422 — the button hides, stray clients get a clean error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translate_reports_disabled_without_backend() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.content.translation_backend = yonder::config::TranslationBackend::LibreTranslate;
    let Some(env) = test_env(config).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;

    let (status, inst) = request(&env.app, "GET", "/api/v2/instance", None, None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inst["configuration"]["translation"]["enabled"], false);

    let (status, post) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&alice),
        Some(json!({"status": "hallo"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{post}");
    let id = post["id"].as_str().unwrap();
    let (status, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/translate"),
        Some(&alice),
        Some(json!({"lang": "en"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("not configured"),
        "{body}"
    );
}

/// `[math]` — LaTeX formulas render through the configured command
/// into real `media_attachments` rows: the status HTML carries an
/// `<img>` per formula and the entity lists them as attachments.
/// Edits reuse the same `math_source` rows instead of regenerating.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn math_formulas_render_as_media_attachments() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.math.enabled = true;
    config.math.command = harness::fake_math_command();
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env.app, "math", "math@example.com").await;

    let (status, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "inline $x^2$ and display $$y^2$$ plus $x^2$ again",
            "content_type": "text/markdown",
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_str().unwrap();
    let html = body["content"].as_str().unwrap();
    assert!(html.contains("data-yonder-math-image=\"inline\""), "{html}");
    assert!(
        html.contains("data-yonder-math-image=\"display\""),
        "{html}"
    );
    // Two distinct formulas → two attachment rows (the repeated
    // `$x^2$` shares the first image).
    let atts = body["media_attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 2, "{body}");
    assert!(atts.iter().all(|a| a["type"] == "image"), "{atts:?}");
    let mut sources: Vec<&str> = atts
        .iter()
        .map(|a| a["yonder"]["math_source"].as_str().unwrap())
        .collect();
    sources.sort();
    assert_eq!(sources, ["display:y^2", "inline:x^2"]);
    // The rendered <img> points at the same public media URL the
    // attachment entity exposes.
    let img_url = atts[0]["url"].as_str().unwrap();
    assert!(html.contains(&format!("src=\"{img_url}\"")), "{html}");
    // Clients that strip <img> (e.g. Mastodon) still get paper-style
    // numbered links to the rendered image — one per occurrence, the
    // repeated `$x^2$` gets its own number. The LaTeX source lives in
    // the img `alt` and the attachment `description`.
    assert!(html.contains("data-yonder-math-alt"), "{html}");
    assert!(html.contains(">(1)</a>"), "{html}");
    assert!(html.contains(">(2)</a>"), "{html}");
    assert!(html.contains(">(3)</a>"), "{html}");
    assert!(html.contains("alt=\"x^2\""), "{html}");
    assert!(!html.contains("\\(x^2\\)"), "{html}");

    // Editing with the same formulas reuses the attachment rows —
    // same ids, no regeneration.
    let (status, edited) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({
            "status": "still $x^2$ and $$y^2$$, edited",
            "content_type": "text/markdown",
        })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let mut edited_ids: Vec<&str> = edited["media_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    edited_ids.sort();
    let mut orig_ids: Vec<&str> = atts.iter().map(|a| a["id"].as_str().unwrap()).collect();
    orig_ids.sort();
    assert_eq!(edited_ids, orig_ids, "{edited}");

    // Removing the formulas detaches the math rows.
    let (status, stripped) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "no math left", "content_type": "text/markdown"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stripped}");
    assert!(
        stripped["media_attachments"].as_array().unwrap().is_empty(),
        "{stripped}"
    );
    assert!(!stripped["content"].as_str().unwrap().contains("<img"));
}

/// Multi-line `$$…$$` blocks with `aligned` environments must reach
/// the renderer verbatim — Markdown's backslash escapes (`\\` → `\`)
/// and the plain-text `<br>` insertion used to corrupt or hide them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn math_multiline_aligned_survives_markdown_and_plain() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.math.enabled = true;
    config.math.command = harness::fake_math_command();
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env.app, "mathml", "mathml@example.com").await;

    // LaTeX row separators (`\\`) and spacing (`\\[4pt]`) — each `\\`
    // below is one escaped backslash pair in the Rust literal.
    let formula =
        "$$\n\\begin{aligned}\n\\text{(A1)}\\quad & a \\\\\n b &= c \\\\[4pt]\n\\end{aligned}\n$$";
    for content_type in ["text/markdown", "text/plain"] {
        let (status, body) = request(
            &env.app,
            "POST",
            "/api/v1/statuses",
            Some(&token),
            Some(json!({
                "status": format!("before\n{formula}\nafter"),
                "content_type": content_type,
            })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{content_type}: {body}");
        let atts = body["media_attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1, "{content_type}: {body}");
        let src = atts[0]["yonder"]["math_source"].as_str().unwrap();
        assert!(src.starts_with("display:"), "{src}");
        let latex = &src["display:".len()..];
        assert!(
            latex.contains("\\\\"),
            "row sep lost ({content_type}): {latex}"
        );
        assert!(
            latex.contains("\\\\[4pt]"),
            "spacing lost ({content_type}): {latex}"
        );
        assert!(latex.contains("\\begin{aligned}"), "{latex}");
        let html = body["content"].as_str().unwrap();
        assert!(
            html.contains("data-yonder-math-image=\"display\""),
            "{content_type}: {html}"
        );
    }
}

// ---------------------------------------------------------------------------
// FEP-8a8e RSVP — inbound Join/Leave on local events and Accept/Reject
// answers to our outbound Joins on remote events (contract Section 10).
// ---------------------------------------------------------------------------

/// Flip a posted status into a local event — Section 7's authoring API
/// doesn't exist yet, so tests set `object_type`/`title` and insert the
/// `events` side row directly.
async fn make_local_event(
    env: &Env,
    token: &str,
    join_mode: &str,
    capacity: Option<i32>,
) -> (i64, String) {
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": "Test event", "visibility": "public"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    let uri = body["uri"].as_str().unwrap().to_string();
    sqlx::query("UPDATE statuses SET object_type = 'Event', title = 'Test event' WHERE id = $1")
        .bind(status_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO events (status_id, start_time, end_time, join_mode, \
                             maximum_attendee_capacity) \
         VALUES ($1, now() + interval '1 day', now() + interval '2 days', $2, $3)",
    )
    .bind(status_id)
    .bind(join_mode)
    .bind(capacity)
    .execute(&env.state.db)
    .await
    .unwrap();
    (status_id, uri)
}

fn join_activity(actor: &str, event_uri: &str) -> (pubby_sdk::Activity, Value) {
    let object = json!({"id": event_uri, "type": "Event"});
    let a = pubby_sdk::Activity {
        id: format!("{actor}/activities/{}", uuid::Uuid::new_v4()),
        activity_type: "Join".into(),
        actor: actor.into(),
        object: object.clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    let raw = json!({
        "id": a.id, "type": "Join", "actor": actor, "object": object,
    });
    (a, raw)
}

fn response_activity(ty: &str, actor: &str, object: Value) -> (pubby_sdk::Activity, Value) {
    let a = pubby_sdk::Activity {
        id: format!("{actor}/activities/{}", uuid::Uuid::new_v4()),
        activity_type: ty.into(),
        actor: actor.into(),
        object: object.clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    let raw = json!({
        "id": a.id, "type": ty, "actor": actor, "object": object,
    });
    (a, raw)
}

async fn participation_state(
    env: &Env,
    event_uri: &str,
    actor_uri: &str,
) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT p.state, p.protocol FROM event_participations p \
         JOIN statuses s ON s.id = p.status_id \
         JOIN accounts a ON a.id = p.account_id \
         WHERE s.uri = $1 AND a.actor_id = $2",
    )
    .bind(event_uri)
    .bind(actor_uri)
    .fetch_optional(&env.state.db)
    .await
    .unwrap()
}

async fn event_counters(env: &Env, status_id: i64) -> (i32, i32) {
    sqlx::query_as::<_, (i32, i32)>(
        "SELECT participants_count, pending_count FROM events WHERE status_id = $1",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap()
}

async fn outbound_count(env: &Env, ty: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM activities WHERE type = $1")
        .bind(ty)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

/// Remote `Join` on a local free event → accepted participation,
/// counters, outbound `Accept`; a duplicate Join stays idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_remote_join_accept() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let token = register(&env.app, "alice", "alice@x.com").await;
    let (status_id, uri) = make_local_event(&env, &token, "free", None).await;
    let bob = "https://remote.test/@bob";

    let (act, raw) = join_activity(bob, &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());

    let p = participation_state(&env, &uri, bob).await;
    assert_eq!(p, Some(("accepted".into(), "join".into())), "{p:?}");
    assert_eq!(event_counters(&env, status_id).await, (1, 0));
    assert!(outbound_count(&env, "Accept").await >= 1);

    // Duplicate Join — still accepted, counters unchanged.
    let (act, raw) = join_activity(bob, &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());
    assert_eq!(event_counters(&env, status_id).await, (1, 0));

    // Leave removes the row and decrements.
    let object = json!({"id": uri, "type": "Event"});
    let a = pubby_sdk::Activity {
        id: format!("{bob}/activities/{}", uuid::Uuid::new_v4()),
        activity_type: "Leave".into(),
        actor: bob.into(),
        object: object.clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    let raw = json!({"id": a.id, "type": "Leave", "actor": bob, "object": object});
    assert!(yonder::events::inbound::handle_activity(&core, &a, &raw)
        .await
        .unwrap());
    assert_eq!(participation_state(&env, &uri, bob).await, None);
    assert_eq!(event_counters(&env, status_id).await, (0, 0));
}

/// Capacity-full `Join` gets `Reject` and no participation row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_join_capacity_reject() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let token = register(&env.app, "alice", "alice@x.com").await;
    let (status_id, uri) = make_local_event(&env, &token, "free", Some(1)).await;

    let (act, raw) = join_activity("https://remote.test/@bob", &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());
    assert_eq!(event_counters(&env, status_id).await, (1, 0));

    let (act, raw) = join_activity("https://remote.test/@carol", &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());
    assert_eq!(
        participation_state(&env, &uri, "https://remote.test/@carol").await,
        None,
        "rejected joiner keeps no row"
    );
    assert_eq!(event_counters(&env, status_id).await, (1, 0));
    assert!(outbound_count(&env, "Reject").await >= 1);
}

/// `Accept`/`TentativeAccept`/`Ignore` answering our outbound `Join`
/// on a remote event; unmatched/spoofed responses are dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_join_responses() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let alice_token = register(&env.app, "alice", "alice@x.com").await;
    let dave_token = register(&env.app, "dave", "dave@x.com").await;
    let alice_actor = format!("{BASE}/ap/users/alice");
    let dave_actor = format!("{BASE}/ap/users/dave");
    let org = "https://remote.test/@org";
    let event_uri = "https://remote.test/events/700";

    // Remote free event materialized by its organizer.
    let create = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/ev700"),
        activity_type: "Create".into(),
        actor: org.into(),
        object: json!({
            "id": event_uri,
            "type": "Event",
            "name": "RSVP target",
            "attributedTo": org,
            "startTime": "2026-08-01T10:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &create, &create.to_json()).await);

    let account_id = |actor: &str| {
        let db = env.state.db.clone();
        let actor = actor.to_string();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE actor_id = $1")
                .bind(actor)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    // Two pending outbound joins, one per local account.
    for (token, actor, req) in [
        (&alice_token, &alice_actor, "req-alice"),
        (&dave_token, &dave_actor, "req-dave"),
    ] {
        let _ = token;
        let status_id: i64 = sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
            .bind(event_uri)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO event_participations \
                 (status_id, account_id, state, protocol, request_activity_uri) \
             VALUES ($1, $2, 'pending', 'join', $3)",
        )
        .bind(status_id)
        .bind(account_id(actor).await)
        .bind(format!("https://remote.test/activities/{req}"))
        .execute(&env.state.db)
        .await
        .unwrap();
    }

    // Spoofed Accept from an unrelated host — an embedded Join
    // echoing alice's actor+event but signed by a third host. Rule 1
    // misses (object id is not a stored request uri) and rule 2
    // requires an authorized responder (evil.test is neither the
    // organizer nor same-host) → dropped.
    let spoof = json!({
        "id": "https://evil.test/activities/spoof1",
        "type": "Accept",
        "actor": "https://evil.test/@mallory",
        "object": {
            "id": "https://remote.test/activities/join-alice",
            "type": "Join",
            "actor": alice_actor,
            "object": event_uri,
        },
    });
    let act = pubby_sdk::Activity {
        id: spoof["id"].as_str().unwrap().into(),
        activity_type: "Accept".into(),
        actor: "https://evil.test/@mallory".into(),
        object: spoof["object"].clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    let _ = yonder::events::inbound::handle_response(
        &core,
        &act,
        &spoof,
        yonder::events::rsvp::RemoteResponseKind::Accept,
    )
    .await
    .unwrap();
    assert_eq!(
        participation_state(&env, event_uri, &alice_actor).await,
        Some(("pending".into(), "join".into())),
        "spoofed embedded-Join Accept dropped"
    );

    // Legit Accept from the organizer matching alice's stored uri.
    let (act, raw) = response_activity(
        "Accept",
        org,
        json!("https://remote.test/activities/req-alice"),
    );
    assert!(yonder::events::inbound::handle_response(
        &core,
        &act,
        &raw,
        yonder::events::rsvp::RemoteResponseKind::Accept,
    )
    .await
    .unwrap());
    assert_eq!(
        participation_state(&env, event_uri, &alice_actor).await,
        Some(("accepted".into(), "join".into()))
    );

    // Friendica-style TentativeAccept matching dave's request uri →
    // tentative.
    let (act, raw) = response_activity(
        "TentativeAccept",
        org,
        json!("https://remote.test/activities/req-dave"),
    );
    assert!(yonder::events::inbound::handle_response(
        &core,
        &act,
        &raw,
        yonder::events::rsvp::RemoteResponseKind::TentativeAccept,
    )
    .await
    .unwrap());
    assert_eq!(
        participation_state(&env, event_uri, &dave_actor).await,
        Some(("tentative".into(), "join".into()))
    );
}

/// `Undo{Join}` removes the *signer's* participation — the embedded
/// Join's self-asserted `actor` must not let a third party delete
/// someone else's row (review C-1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_undo_join_uses_verified_signer() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let token = register(&env.app, "alice", "alice@x.com").await;
    let (status_id, uri) = make_local_event(&env, &token, "free", None).await;
    let victim = "https://remote.test/@victim";
    let attacker = "https://remote.test/@attacker";

    // Victim joins and is accepted.
    let (act, raw) = join_activity(victim, &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());
    assert_eq!(event_counters(&env, status_id).await, (1, 0));

    // Attacker signs an Undo whose embedded Join names the victim.
    let inner = json!({
        "id": format!("{victim}/activities/join-1"),
        "type": "Join",
        "actor": victim,
        "object": {"id": uri, "type": "Event"},
    });
    let outer = pubby_sdk::Activity {
        id: format!("{attacker}/activities/undo-1"),
        activity_type: "Undo".into(),
        actor: attacker.into(),
        object: inner.clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(
        yonder::events::inbound::handle_undo_join(&core, &outer, &inner)
            .await
            .unwrap()
    );

    // The victim's row and the counter survive — the attacker had no
    // participation to remove.
    assert_eq!(
        participation_state(&env, &uri, victim).await,
        Some(("accepted".into(), "join".into()))
    );
    assert_eq!(participation_state(&env, &uri, attacker).await, None);
    assert_eq!(event_counters(&env, status_id).await, (1, 0));

    // The victim's own Undo does remove their row.
    let inner = json!({
        "id": format!("{victim}/activities/join-1"),
        "type": "Join",
        "actor": victim,
        "object": {"id": uri, "type": "Event"},
    });
    let outer = pubby_sdk::Activity {
        id: format!("{victim}/activities/undo-2"),
        activity_type: "Undo".into(),
        actor: victim.into(),
        object: inner.clone(),
        to: vec![],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(
        yonder::events::inbound::handle_undo_join(&core, &outer, &inner)
            .await
            .unwrap()
    );
    assert_eq!(participation_state(&env, &uri, victim).await, None);
    assert_eq!(event_counters(&env, status_id).await, (0, 0));
}

/// A `pending` join on a restricted event cannot self-approve with
/// `Accept` — the row stays pending for the organizer (review C-2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_pending_join_cannot_self_accept() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let token = register(&env.app, "alice", "alice@x.com").await;
    let (status_id, uri) = make_local_event(&env, &token, "restricted", None).await;
    let bob = "https://remote.test/@bob";

    let (act, raw) = join_activity(bob, &uri);
    assert!(yonder::events::inbound::handle_activity(&core, &act, &raw)
        .await
        .unwrap());
    assert_eq!(
        participation_state(&env, &uri, bob).await,
        Some(("pending".into(), "join".into()))
    );
    assert_eq!(event_counters(&env, status_id).await, (0, 1));

    // Bob's own Accept{object: event} must not resolve the pending row.
    let (act, raw) = response_activity("Accept", bob, json!(uri));
    assert!(yonder::events::inbound::handle_response(
        &core,
        &act,
        &raw,
        yonder::events::rsvp::RemoteResponseKind::Accept,
    )
    .await
    .unwrap());
    assert_eq!(
        participation_state(&env, &uri, bob).await,
        Some(("pending".into(), "join".into())),
        "pending join cannot self-approve"
    );
    assert_eq!(event_counters(&env, status_id).await, (0, 1));
}

/// Raw GET returning status + body text (ICS feeds aren't JSON).
async fn get_text(app: &Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

/// ICS export: per-event route serves anonymously for public events
/// (and 404s for postponed ones); the user feed lists upcoming public
/// events (review C-3, W-2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_ics_routes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "alice", "alice@x.com").await;
    let (status_id, _uri) = make_local_event(&env, &token, "free", None).await;

    // Anonymous per-event ICS.
    let (code, body) = get_text(&env.app, &format!("/api/v1/yonder/events/{status_id}/ics")).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains("BEGIN:VCALENDAR"), "{body}");
    assert!(body.contains("BEGIN:VEVENT"), "{body}");
    assert!(body.contains("SUMMARY:Test event"), "{body}");

    // User calendar feed — the advertised discovery URL shape.
    let (code, body) = get_text(&env.app, "/users/alice/events.ics").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(body.contains("BEGIN:VCALENDAR"), "{body}");
    assert!(body.contains("SUMMARY:Test event"), "{body}");

    // Postponed (no start) → the per-event route 404s. `end_time`
    // stays set so the user-feed query still selects the row and the
    // writer's skip-before-VEVENT path is what drops it.
    sqlx::query("UPDATE events SET start_time = NULL, status = 'postponed' WHERE status_id = $1")
        .bind(status_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (code, _) = get_text(&env.app, &format!("/api/v1/yonder/events/{status_id}/ics")).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // …and the user feed emits no empty VEVENT for it.
    let (code, body) = get_text(&env.app, "/users/alice/events.ics").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(!body.contains("BEGIN:VEVENT"), "{body}");
}

/// Section 7 — local event authoring: `post_type=event` + `event`
/// payload materializes `object_type='Event'`, `statuses.title`, the
/// `events` side table and the organizer participation row; the
/// status entity exposes `yonder.event`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_local_event() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "orga", "orga@x.com").await;

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "Come to the **meetup**",
            "post_type": "event",
            "visibility": "public",
            "event": {
                "start_time": "2030-06-01T18:00:00+02:00",
                "end_time": "2030-06-01T20:00:00+02:00",
                "timezone": "Europe/Rome",
                "join_mode": "free",
                "categories": ["MEETING", "NETWORKING"],
                "locations": [{"type": "place", "name": "Cafe Central", "locality": "Rome"}],
                "maximum_attendee_capacity": 50,
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["yonder"]["event"]["name"], "Come to the **meetup**");
    assert_eq!(body["yonder"]["event"]["status"], "scheduled");
    assert_eq!(body["yonder"]["event"]["join_mode"], "free");
    // The organizer's own participation row is accepted → join_state.
    assert_eq!(body["pleroma"]["event"]["join_state"], "accept");

    // Side table: object_type/title + event row + organizer row.
    let row: (String, String, String, i32) = sqlx::query_as(
        "SELECT s.object_type, s.title, e.status, e.maximum_attendee_capacity \
         FROM statuses s JOIN events e ON e.status_id = s.id WHERE s.id = $1",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "Event".into(),
            "Come to the **meetup**".into(),
            "scheduled".into(),
            50
        )
    );
    let organizer: String = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND role = 'organizer'",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(organizer, "accepted");
    let cats: Vec<String> =
        sqlx::query_scalar("SELECT categories FROM events WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(cats, vec!["MEETING".to_string(), "NETWORKING".to_string()]);

    // Edit: postpone (status change is an Update, not a delete).
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({
            "status": "Come to the **meetup**",
            "event": {"status": "postponed", "timezone": "Europe/Rome"}
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let st: String = sqlx::query_scalar("SELECT status FROM events WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(st, "postponed");
    // Postponed drops the start time.
    let has_start: bool =
        sqlx::query_scalar("SELECT start_time IS NOT NULL FROM events WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!has_start);
}

/// Section 7 — reschedule auto-fills `previous_start_time` from the
/// stored start; an explicit organizer value wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reschedule_event_fills_previous_start() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "resched", "resched@x.com").await;
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "Meetup",
            "post_type": "event",
            "event": {
                "start_time": "2030-06-01T18:00:00Z",
                "timezone": "UTC",
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&token),
        Some(json!({
            "status": "Meetup",
            "event": {
                "start_time": "2030-06-08T18:00:00Z",
                "timezone": "UTC",
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (status, prev): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, previous_start_time FROM events WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(status, "rescheduled");
    assert_eq!(
        prev.map(|t| t.to_rfc3339()),
        Some("2030-06-01T18:00:00+00:00".to_string())
    );
}

/// Section 7 — validation failures on local event authoring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_validation_errors() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "val", "val@x.com").await;
    let post = |event: Value, extra: Value| {
        let mut body = json!({"status": "x", "post_type": "event"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body["event"] = event;
        body
    };
    let try_post = |payload: Value| {
        let app = env.app.clone();
        let token = token.clone();
        async move {
            request(
                &app,
                "POST",
                "/api/v1/statuses",
                Some(&token),
                Some(payload),
                &[],
            )
            .await
        }
    };

    // Missing title.
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC"}),
        json!({"status": ""}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Missing start_time (non-postponed).
    let (code, _) = try_post(post(json!({"timezone": "UTC"}), json!({"status": "x"}))).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Bad timezone.
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "Mars/Olympus"}),
        json!({"status": "x"}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Poll on an event.
    let (code, _) = try_post(json!({
        "status": "x", "post_type": "event",
        "poll": {"options": ["a", "b"], "expires_in": 3600},
        "event": {"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC"},
    }))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // external join_mode without URL.
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC", "join_mode": "external"}),
        json!({"status": "x"}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Non-FEP category (allow_custom_categories off by default).
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC", "categories": ["MY_CUSTOM"]}),
        json!({"status": "x"}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Too many locations (default cap 5).
    let locs: Vec<Value> = (0..6)
        .map(|i| json!({"name": format!("Loc {i}")}))
        .collect();
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC", "locations": locs}),
        json!({"status": "x"}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Invitees on a non-direct event.
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "x", "post_type": "event", "visibility": "public",
            "event": {
                "start_time": "2030-01-01T10:00:00Z", "timezone": "UTC",
                "invitees": [1],
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Cancelled status on create is rejected (only scheduled/tentative).
    let (code, _) = try_post(post(
        json!({"start_time": "2030-01-01T10:00:00Z", "timezone": "UTC", "status": "cancelled"}),
        json!({"status": "x"}),
    ))
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Section 11 — the Yonder-native RSVP API: join/leave through the
/// §7.1 state machine, organizer guards, pending-request decisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_rsvp_api() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let org_token = register(&env.app, "orga", "orga@x.com").await;
    let bob_token = register(&env.app, "bob", "bob@x.com").await;
    let bob_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'bob' AND domain IS NULL")
            .fetch_one(&env.state.db)
            .await
            .unwrap();

    // A restricted event: joins land pending until the organizer decides.
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&org_token),
        Some(json!({
            "status": "Gated meetup",
            "post_type": "event",
            "visibility": "public",
            "event": {
                "start_time": "2030-07-01T18:00:00Z",
                "timezone": "UTC",
                "join_mode": "restricted",
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Bob joins → pending (restricted requires organizer approval).
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/rsvp"),
        Some(&bob_token),
        Some(json!({"state": "accepted", "message": "see you there"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["participation"]["state"], "pending");
    let (accepted, pending) = event_counters(&env, status_id).await;
    assert_eq!((accepted, pending), (0, 1));

    // The organizer cannot RSVP to their own event.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/rsvp"),
        Some(&org_token),
        Some(json!({"state": "accepted"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Organizer lists pending requests, then authorizes.
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/yonder/events/{status_id}/participations"),
        Some(&org_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let rows = body["participations"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "pending");
    assert_eq!(rows[0]["participation_message"], "see you there");

    // A non-organizer cannot decide.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/participation_requests/{bob_id}/authorize"),
        Some(&bob_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/participation_requests/{bob_id}/authorize"),
        Some(&org_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (accepted, pending) = event_counters(&env, status_id).await;
    assert_eq!((accepted, pending), (1, 0));

    // Bob leaves → the participation row is withdrawn.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/rsvp"),
        Some(&bob_token),
        Some(json!({"state": "none"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(bob_id)
    .fetch_optional(&env.state.db)
    .await
    .unwrap()
    .flatten();
    assert!(
        state.is_none() || state.as_deref() == Some("declined"),
        "{state:?}"
    );
}

/// Regression — RSVP'ing a *remote* event through the local API
/// 404'd with "account not found": the organizer lookup went through
/// the local-only `actor_of`, but a remote event's author is a remote
/// account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_event_rsvp_api() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "joiner", "joiner@x.com").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();

    // A free-join remote event materialized by its organizer.
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/remotersvp"),
        activity_type: "Create".into(),
        actor: "https://remote.test/@org".into(),
        object: json!({
            "id": "https://remote.test/events/rsvp1",
            "type": "Event",
            "name": "Remote RSVP test",
            "attributedTo": "https://remote.test/@org",
            "startTime": "2030-03-01T10:00:00Z",
            "joinMode": "free",
            "content": "<p>come along</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    remote::handle(&core, &activity, &activity.to_json()).await;
    let status_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE uri = 'https://remote.test/events/rsvp1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // The RSVP succeeds (the outbound Join can't deliver to
    // remote.test, but publish failures are logged, not fatal). The
    // row pends until the home instance's Accept round-trips back.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/yonder/events/{status_id}/rsvp"),
        Some(&token),
        Some(json!({"state": "accepted"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["participation"]["state"], "pending");
}

/// Section 11 — the Nicolex `/api/v1/pleroma/events*` adapter:
/// create/joined_events/join/participations/requests/ics/location.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_events_adapter() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let org_token = register(&env.app, "norganizer", "norg@x.com").await;
    let bob_token = register(&env.app, "nbob", "nbob@x.com").await;
    let bob_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'nbob' AND domain IS NULL")
            .fetch_one(&env.state.db)
            .await
            .unwrap();

    // Geocoding is off → location search returns [].
    let (code, body) = request(
        &env.app,
        "GET",
        "/api/v1/pleroma/search/location?q=rome",
        Some(&org_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    // Create through the Nicolex param shape.
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/pleroma/events",
        Some(&org_token),
        Some(json!({
            "name": "Nicolex Meetup",
            "status": "Adapter-created event",
            "start_time": "2030-08-01T18:00:00Z",
            "end_time": "2030-08-01T20:00:00Z",
            "join_mode": "free",
            "attendees_visibility": "public",
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(body["pleroma"]["event"]["name"], "Nicolex Meetup");

    // `location_id` without geocoding → 422 with a hint.
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/pleroma/events",
        Some(&org_token),
        Some(json!({
            "name": "Geo event",
            "start_time": "2030-08-01T18:00:00Z",
            "location_id": 12345,
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Bob joins through the adapter.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/pleroma/events/{status_id}/join"),
        Some(&bob_token),
        Some(json!({"participation_message": "hi"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let st: String = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(bob_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(st, "accepted");

    // joined_events lists it for Bob.
    let (code, body) = request(
        &env.app,
        "GET",
        "/api/v1/pleroma/events/joined_events",
        Some(&bob_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&status_id.to_string().as_str()), "{ids:?}");

    // participations returns Bob's account entity (public visibility).
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/pleroma/events/{status_id}/participations"),
        Some(&bob_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let accts = body.as_array().unwrap();
    assert!(accts.iter().any(|a| a["acct"] == "nbob"), "{accts:?}");

    // participation_requests is organizer-only → Bob gets 403.
    let (code, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/pleroma/events/{status_id}/participation_requests"),
        Some(&bob_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    // ICS through the adapter path.
    let (code, text) = get_text(&env.app, &format!("/api/v1/pleroma/events/{status_id}/ics")).await;
    assert_eq!(code, StatusCode::OK, "{text}");
    assert!(text.contains("BEGIN:VCALENDAR"), "{text}");

    // Leave withdraws the participation.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/pleroma/events/{status_id}/leave"),
        Some(&bob_token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

/// Nicolex `PUT` with only `start_time` must merge over the stored
/// event — locations, categories, join mode, timezone and invitees
/// survive (review C-1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_events_partial_edit_preserves_metadata() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "npartial", "npartial@x.com").await;

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/pleroma/events",
        Some(&token),
        Some(json!({
            "name": "Full Event",
            "status": "desc",
            "start_time": "2030-08-01T18:00:00Z",
            "end_time": "2030-08-01T20:00:00Z",
            "timezone": "Europe/Rome",
            "join_mode": "restricted",
            "attendees_visibility": "participants",
            "categories": ["MEETING"],
            "locations": [{"type": "place", "name": "Hall", "locality": "Rome"}],
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Partial edit: only the start moves.
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/pleroma/events/{status_id}"),
        Some(&token),
        Some(json!({"start_time": "2030-08-02T18:00:00Z"})),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let row: (String, String, String, Vec<String>, bool) = sqlx::query_as(
        "SELECT e.timezone, e.join_mode, e.attendees_visibility, e.categories, \
                (SELECT count(*) FROM event_locations l WHERE l.status_id = e.status_id) > 0 \
         FROM events e WHERE e.status_id = $1",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(row.0, "Europe/Rome");
    assert_eq!(row.1, "restricted");
    assert_eq!(row.2, "participants");
    assert_eq!(row.3, vec!["MEETING".to_string()]);
    assert!(row.4, "location survived partial edit");
    let moved: bool = sqlx::query_scalar(
        "SELECT start_time = '2030-08-02 18:00:00+00'::timestamptz FROM events WHERE status_id = $1",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(moved);
    // Name/description untouched.
    let title: String = sqlx::query_scalar("SELECT title FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(title, "Full Event");
}

/// `GET /api/v1/yonder/events?from=<past>` must not collapse the
/// window to `from + 90d` — an explicit `from` without `to` is
/// unbounded (review C-2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_list_explicit_from_is_unbounded() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "elist", "elist@x.com").await;
    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({
            "status": "Old meetup",
            "post_type": "event",
            "visibility": "public",
            "event": {
                "start_time": "2020-01-01T10:00:00Z",
                "timezone": "UTC",
                "join_mode": "free",
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = request(
        &env.app,
        "GET",
        "/api/v1/yonder/events?scope=mine&from=2000-01-01T00:00:00Z&include_cancelled=true",
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{body}");
    assert_eq!(arr[0]["yonder"]["event"]["name"], "Old meetup");
}

/// Discovery scope visibility (review-04 C-1/C-2): `federated` returns
/// public events from anywhere (remote included) and `local` returns
/// public + local-visibility events from this instance — neither ever
/// includes `unlisted` (plan §8: calendar/search output must carry
/// Public in `to`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_list_discovery_scopes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "dscalice", "dscalice@example.com").await;
    // Start times must sit inside the list API's default window
    // (now → +90d).
    let start = (chrono::Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let make_event = |title: &'static str, visibility: &'static str| {
        let app = env.app.clone();
        let token = token.clone();
        let start = start.clone();
        async move {
            let (code, body) = request(
                &app,
                "POST",
                "/api/v1/statuses",
                Some(&token),
                Some(json!({
                    "status": title,
                    "post_type": "event",
                    "visibility": visibility,
                    "event": {
                        "start_time": start,
                        "timezone": "UTC",
                        "join_mode": "free",
                    }
                })),
                &[],
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{body}");
            let id: i64 = body["id"].as_str().unwrap().parse().unwrap();
            id
        }
    };
    let public_id = make_event("Public discovery", "public").await;
    let unlisted_id = make_event("Unlisted discovery", "unlisted").await;
    let local_id = make_event("Local discovery", "local").await;

    // A remote public event, materialized through the ingest path.
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_uri = "https://remote.test/events/discovery";
    let activity = pubby_sdk::Activity {
        id: format!("{BASE}/ap/activities/discovery"),
        activity_type: "Create".into(),
        actor: "https://remote.test/@host".into(),
        object: json!({
            "id": remote_uri,
            "type": "Event",
            "name": "Remote discovery",
            "attributedTo": "https://remote.test/@host",
            "startTime": start,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
        to: vec!["https://www.w3.org/ns/activitystreams#Public".into()],
        cc: vec![],
        published: None,
        signature: None,
    };
    remote::handle(&core, &activity, &activity.to_json()).await;
    let remote_id: i64 = sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
        .bind(remote_uri)
        .fetch_one(&env.state.db)
        .await
        .unwrap();

    let scope_ids = |scope: &'static str, tok: Option<String>| {
        let app = env.app.clone();
        async move {
            let (code, body) = request(
                &app,
                "GET",
                &format!("/api/v1/yonder/events?scope={scope}&limit=40"),
                tok.as_deref(),
                None,
                &[],
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{body}");
            let arr = body.as_array().unwrap();
            arr.iter()
                .map(|s| s["id"].as_str().unwrap().parse::<i64>().unwrap())
                .collect::<Vec<i64>>()
        }
    };
    let tok = token.clone();

    // Federated: public everywhere — local public + remote, never
    // unlisted or local-visibility.
    let ids = scope_ids("federated", Some(tok.clone())).await;
    assert!(ids.contains(&public_id), "{ids:?}");
    assert!(ids.contains(&remote_id), "{ids:?}");
    assert!(!ids.contains(&unlisted_id), "{ids:?}");
    assert!(!ids.contains(&local_id), "{ids:?}");

    // Local: public + local-visibility from this instance only.
    let ids = scope_ids("local", Some(tok.clone())).await;
    assert!(ids.contains(&public_id), "{ids:?}");
    assert!(ids.contains(&local_id), "{ids:?}");
    assert!(!ids.contains(&unlisted_id), "{ids:?}");
    assert!(!ids.contains(&remote_id), "{ids:?}");

    // Anonymous: same exclusions (unlisted must not leak).
    let ids = scope_ids("local", None).await;
    assert!(ids.contains(&public_id), "{ids:?}");
    assert!(!ids.contains(&unlisted_id), "{ids:?}");
    assert!(!ids.contains(&local_id), "{ids:?}");
    let ids = scope_ids("federated", None).await;
    assert!(
        ids.contains(&public_id) && ids.contains(&remote_id),
        "{ids:?}"
    );
    assert!(!ids.contains(&unlisted_id), "{ids:?}");
}

/// Discovery filters (review-04 T-1): `near` bounding box + distance
/// check including the antimeridian wrap, the hashtag filter, and the
/// virtual-only `online` filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_list_geo_tag_and_online_filters() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "geofilt", "geofilt@example.com").await;
    // Inside the list API's default window (now → +90d).
    let start = (chrono::Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let make = |content: &'static str, locations: Value| {
        let app = env.app.clone();
        let token = token.clone();
        let start = start.clone();
        async move {
            let (code, body) = request(
                &app,
                "POST",
                "/api/v1/statuses",
                Some(&token),
                Some(json!({
                    "status": content,
                    "post_type": "event",
                    "visibility": "public",
                    "event": {
                        "start_time": start,
                        "timezone": "UTC",
                        "join_mode": "free",
                        "locations": locations,
                    }
                })),
                &[],
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{body}");
            body["id"].as_str().unwrap().parse::<i64>().unwrap()
        }
    };
    let milan = make(
        "Milan meetup #geofiltertag",
        json!([{"type": "place", "name": "Duomo", "latitude": 45.4642, "longitude": 9.19}]),
    )
    .await;
    let oslo = make(
        "Oslo meetup",
        json!([{"type": "place", "name": "Opera", "latitude": 59.9139, "longitude": 10.7522}]),
    )
    .await;
    // Just west of the antimeridian: the longitude box would have to
    // wrap past ±180°, so only the latitude band applies.
    let dateline = make(
        "Dateline meetup",
        json!([{"type": "place", "name": "Atoll", "latitude": 0.0, "longitude": -179.95}]),
    )
    .await;
    let virtual_event = make(
        "Online meetup",
        json!([{"type": "virtual", "name": "Room", "url": "https://meet.example/room"}]),
    )
    .await;

    let ids = |query: &'static str| {
        let app = env.app.clone();
        let token = token.clone();
        async move {
            let (code, body) = request(
                &app,
                "GET",
                &format!("/api/v1/yonder/events?scope=mine&limit=40&{query}"),
                Some(&token),
                None,
                &[],
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{body}");
            body.as_array()
                .unwrap()
                .iter()
                .map(|s| s["id"].as_str().unwrap().parse::<i64>().unwrap())
                .collect::<Vec<i64>>()
        }
    };

    // Near Milan, 25 km: the Milan event only — Oslo is far, the
    // virtual event has no physical location.
    let near_milan = ids("near=45.4642,9.19&radius_km=25").await;
    assert!(near_milan.contains(&milan), "{near_milan:?}");
    assert!(!near_milan.contains(&oslo), "{near_milan:?}");
    assert!(!near_milan.contains(&virtual_event), "{near_milan:?}");

    // Antimeridian: a query just east of ±180° must still find the
    // event just west of it (the longitude box is skipped, the
    // distance check still applies).
    let near_dateline = ids("near=0,179.95&radius_km=50").await;
    assert!(near_dateline.contains(&dateline), "{near_dateline:?}");
    assert!(!near_dateline.contains(&milan), "{near_dateline:?}");

    // Hashtag filter is case-insensitive and matches the parsed tag.
    let tagged = ids("tag=GeoFilterTag").await;
    assert_eq!(tagged, vec![milan], "{tagged:?}");

    // `online=true` keeps only events with a virtual location and no
    // physical one.
    let online_only = ids("online=true").await;
    assert_eq!(online_only, vec![virtual_event], "{online_only:?}");
}

/// Personal calendar feed ordering (review-04 R-3): upcoming events
/// come first (nearest start first), then past events most recent
/// first, so an overflowing 1000-row cap drops the oldest past events
/// rather than the imminent ones. Postponed events never render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calendar_feed_orders_upcoming_first() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "calorder", "calorder@example.com").await;
    let (past, _) = make_local_event(&env, &token, "free", None).await;
    let (soon, _) = make_local_event(&env, &token, "free", None).await;
    let (later, _) = make_local_event(&env, &token, "free", None).await;
    let (postponed, _) = make_local_event(&env, &token, "free", None).await;
    for (id, title, offset) in [
        (past, "Past event", "-2 days"),
        (soon, "Soon event", "1 day"),
        (later, "Later event", "10 days"),
    ] {
        sqlx::query("UPDATE statuses SET title = $2 WHERE id = $1")
            .bind(id)
            .bind(title)
            .execute(&env.state.db)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE events SET start_time = now() + $2::interval, \
                               end_time = now() + $2::interval + interval '1 hour' \
             WHERE status_id = $1",
        )
        .bind(id)
        .bind(offset)
        .execute(&env.state.db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE events SET start_time = NULL, status = 'postponed' WHERE status_id = $1")
        .bind(postponed)
        .execute(&env.state.db)
        .await
        .unwrap();

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/yonder/calendar/token",
        Some(&token),
        None,
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let url = body["calendar_url"].as_str().unwrap().to_string();
    let token_part = url.rsplit('/').next().unwrap().trim_end_matches(".ics");
    let (code, feed) = get_text(
        &env.app,
        &format!("/users/calorder/calendar/{token_part}.ics"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{feed}");
    let order: Vec<&str> = feed
        .lines()
        .filter_map(|l| l.strip_prefix("SUMMARY:"))
        .collect();
    assert_eq!(
        order,
        vec!["Soon event", "Later event", "Past event"],
        "{feed}"
    );
    // The postponed event keeps its original title and never renders.
    assert!(!feed.contains("SUMMARY:Test event"), "{feed}");
}

/// `upcomingEvents` collection (review-04 T-1): ascending by start
/// time, public + unlisted only, postponed and tombstoned events
/// excluded, unknown accounts 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upcoming_events_collection_scopes_and_order() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "upalice", "upalice@example.com").await;
    let start = (chrono::Utc::now() + chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let make = |title: &'static str, visibility: &'static str| {
        let app = env.app.clone();
        let token = token.clone();
        let start = start.clone();
        async move {
            let (code, body) = request(
                &app,
                "POST",
                "/api/v1/statuses",
                Some(&token),
                Some(json!({
                    "status": title,
                    "post_type": "event",
                    "visibility": visibility,
                    "event": {
                        "start_time": start,
                        "timezone": "UTC",
                        "join_mode": "free",
                    }
                })),
                &[],
            )
            .await;
            assert_eq!(code, StatusCode::OK, "{body}");
            (
                body["id"].as_str().unwrap().parse::<i64>().unwrap(),
                body["uri"].as_str().unwrap().to_string(),
            )
        }
    };
    let (soonest, soonest_uri) = make("Soonest", "public").await;
    let (middle, middle_uri) = make("Middle", "unlisted").await;
    let (latest, latest_uri) = make("Latest", "public").await;
    let (private, _) = make("Private", "private").await;
    let (postponed, _) = make("Postponed", "public").await;
    let (tombstoned, _) = make("Tombstoned", "public").await;
    for (id, offset) in [
        (soonest, "1 day"),
        (middle, "2 days"),
        (latest, "3 days"),
        (private, "4 days"),
        (tombstoned, "5 days"),
    ] {
        sqlx::query(
            "UPDATE events SET start_time = now() + $2::interval, \
                               end_time = now() + $2::interval + interval '1 hour' \
             WHERE status_id = $1",
        )
        .bind(id)
        .bind(offset)
        .execute(&env.state.db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE events SET start_time = NULL, status = 'postponed' WHERE status_id = $1")
        .bind(postponed)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET tombstone = true WHERE id = $1")
        .bind(tombstoned)
        .execute(&env.state.db)
        .await
        .unwrap();

    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get("/ap/users/upalice/collections/upcoming_events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(doc["type"], "OrderedCollection");
    let ids: Vec<String> = doc["orderedItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![soonest_uri, middle_uri, latest_uri], "{doc}");
    assert_eq!(doc["totalItems"], 3);

    // Unknown account → 404 (cacheable miss, not a 500).
    let (code, _) = get_text(&env.app, "/ap/users/nosuchuser/collections/upcoming_events").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

/// Invite fan-out (review-04 T-1): a local event with `invitees` gets
/// one `invited` participation row and one `Invite` activity per
/// invitee; a decided state is never overwritten and only newly added
/// invitees get an `Invite` on edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_event_invite_fanout() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let organizer = register(&env.app, "invorg", "invorg@example.com").await;
    let _bob = register(&env.app, "invbob", "invbob@example.com").await;
    let _carol = register(&env.app, "invcarol", "invcarol@example.com").await;
    let bob_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'invbob'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let carol_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'invcarol'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let start = (chrono::Utc::now() + chrono::Duration::days(5))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&organizer),
        Some(json!({
            "status": "Invite-only planning",
            "post_type": "event",
            "visibility": "direct",
            "event": {
                "start_time": start,
                "timezone": "UTC",
                "join_mode": "invite",
                "invitees": [bob_id],
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let status_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let invited: String = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(bob_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(invited, "invited");
    assert_eq!(outbound_count(&env, "Invite").await, 1);

    // Bob decides. A later edit that still lists him must not reset
    // the state and must not re-send the Invite.
    sqlx::query(
        "UPDATE event_participations SET state = 'accepted' \
         WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(bob_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&organizer),
        Some(json!({
            "status": "Invite-only planning (updated)",
            "event": {
                "start_time": start,
                "timezone": "UTC",
                "join_mode": "invite",
                "invitees": [bob_id],
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let state: String = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(bob_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(state, "accepted");
    assert_eq!(outbound_count(&env, "Invite").await, 1);

    // Adding an invitee sends exactly one more Invite.
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&organizer),
        Some(json!({
            "status": "Invite-only planning (updated)",
            "event": {
                "start_time": start,
                "timezone": "UTC",
                "join_mode": "invite",
                "invitees": [bob_id, carol_id],
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(outbound_count(&env, "Invite").await, 2);
    let carol: String = sqlx::query_scalar(
        "SELECT state FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(carol_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(carol, "invited");

    // Removing an undecided invitee drops the `invited` row; the
    // decided one keeps hers.
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{status_id}"),
        Some(&organizer),
        Some(json!({
            "status": "Invite-only planning (updated)",
            "event": {
                "start_time": start,
                "timezone": "UTC",
                "join_mode": "invite",
                "invitees": [bob_id],
            }
        })),
        &[],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT account_id, state FROM event_participations \
         WHERE status_id = $1 AND role = 'participant' ORDER BY account_id",
    )
    .bind(status_id)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(rows, vec![(bob_id, "accepted".to_string())], "{rows:?}");
}

/// Actor document (review-05 T-1): `upcomingEvents` is injected after
/// pubby's dispatch, so the document must carry a validator for the
/// injected body and never answer a conditional GET with `304` — a
/// pubby-computed `If-None-Match` match would freeze the stale body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_document_advertises_upcoming_and_never_304s() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env.app, "etagalice", "etagalice@example.com").await;

    let get = |inm: Option<String>| {
        let app = env.app.clone();
        async move {
            let mut req = Request::get("/ap/users/etagalice")
                .header(axum::http::header::ACCEPT, "application/activity+json");
            if let Some(v) = inm {
                req = req.header(axum::http::header::IF_NONE_MATCH, v);
            }
            let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            let status = resp.status();
            let etag = resp
                .headers()
                .get(axum::http::header::ETAG)
                .map(|v| v.to_str().unwrap().to_string());
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, etag, bytes)
        }
    };

    let (status, etag, bytes) = get(None).await;
    assert_eq!(status, StatusCode::OK);
    let etag = etag.expect("actor document carries an ETag");
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        doc["upcomingEvents"],
        format!("{BASE}/ap/users/etagalice/collections/upcoming_events")
    );

    // The validator from the first response must not produce a 304:
    // the actor document is never served conditionally.
    let (status, _, bytes) = get(Some(etag)).await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(doc["upcomingEvents"].is_string());
}
