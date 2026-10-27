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
