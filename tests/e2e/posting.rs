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
    jobs::{queue, JobContext, JobEnvelope, JobHandler, JobKind},
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
    assert!(html.contains("<strong>When a Child’s Kite"));
    assert!(html.contains("<p>[…]</p>"), "teaser in text: {html}");
    assert!(
        !html.contains("<img"),
        "remote img must be stripped: {html}"
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

/// Drain every pending entry from the `default` job stream and return
/// the decoded envelopes — the test stands in for the worker.
async fn drain_default_queue(env: &Env) -> Vec<JobEnvelope> {
    drain_queue(env, "default").await
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
            out.push(queue::decode(&body).unwrap());
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

    job.run(&jctx, &json!({})).await.unwrap();
    let scan: i32 =
        sqlx::query_scalar("SELECT scan_version FROM preview_cards WHERE status_id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(scan, 1, "sweep must mark the row scanned");
    let jobs = drain_queue(&env, "bulk").await;
    let swept: Vec<_> = jobs
        .iter()
        .filter(|j| j.kind == JobKind::FetchLinkPreview)
        .collect();
    // Only the stale row fans out — the scan_version=1 cards written
    // above are not re-enqueued.
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
