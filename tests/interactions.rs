//! Epic-13 interactions integration tests — favourite, boost,
//! bookmark, quote, emoji reactions, pins and the inbound AP arms,
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
    interactions,
    jobs::{JobContext, JobHandler},
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
        .join(format!("yonder-int-{}", uuid::Uuid::new_v4()))
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
    let json = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

/// Create a status and return (id, uri).
async fn post(app: &Router, token: &str, status: &str, visibility: &str) -> (String, String) {
    let (code, body) = request(
        app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": status, "visibility": visibility})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    (
        body["id"].as_str().unwrap().to_string(),
        body["uri"].as_str().unwrap().to_string(),
    )
}

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
        "object": object,
    });
    (a, raw)
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn favourite_roundtrip_and_undo() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "likeable", "public").await;

    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["favourited"], true);
    assert_eq!(body["favourites_count"], 1);

    // Idempotent double-like.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/favourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["favourites_count"], 1);

    // favourited_by + /favourites listing.
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/favourited_by"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["username"], "bob");
    // Anonymous reads are allowed on public statuses.
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/favourited_by"),
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["username"], "bob");
    let (code, body) = request(&env.app, "GET", "/api/v1/favourites", Some(&bob), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["id"], id);

    // Unfavourite — decrements, idempotent.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unfavourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["favourited"], false);
    assert_eq!(body["favourites_count"], 0);
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unfavourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boost_roundtrip_visibility_and_undo() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "boost me", "public").await;

    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    // The boost entity embeds the original under `reblog`.
    assert_eq!(body["reblog"]["id"], id);
    assert_eq!(body["reblog"]["reblogs_count"], 1);
    assert_eq!(body["reblogged"], false);

    // Replay returns the same boost.
    let (code, body2) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body2["id"], body["id"]);

    // Followers-only and direct are not boostable.
    let (private_id, _) = post(&env.app, &alice, "private post", "private").await;
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{private_id}/reblog"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/reblogged_by"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["username"], "bob");

    // Unreblog — boost row tombstoned, counter decremented, flag off.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unreblog"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["reblogs_count"], 0);
    assert_eq!(body["reblogged"], false);
    // Repeat unreblog is a clean no-op.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unreblog"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

/// A stale `unreblog_row` — a duplicate call or racing `Undo` landing
/// on an already-tombstoned boost — must not decrement the counter a
/// second time (a concurrent boost's count would be stolen).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreblog_stale_row_keeps_counter() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let (id, _) = post(&env.app, &alice, "count me", "public").await;

    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let boost_id = body["id"].as_str().unwrap().parse::<i64>().unwrap();
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/reblog"),
        Some(&carol),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // First call tombstones + decrements (2 → 1); the replay on the
    // now-tombstoned row must be a no-op.
    let ctx = yonder::posts::PostContext::new(&env.state);
    let boost = yonder::posts::load(&ctx.db, boost_id)
        .await
        .unwrap()
        .unwrap();
    interactions::boost::unreblog_row(&ctx, &boost)
        .await
        .unwrap();
    interactions::boost::unreblog_row(&ctx, &boost)
        .await
        .unwrap();

    let count: i32 = sqlx::query_scalar("SELECT reblogs_count FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// `visibility` on reblog is accepted from the JSON body, a form body
/// and the query string (Mastodon clients use all three).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reblog_visibility_from_body_and_query() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let carol = register(&env.app, "carol", "c@x.com").await;
    let (id1, _) = post(&env.app, &alice, "via body", "public").await;
    let (id2, _) = post(&env.app, &alice, "via query", "public").await;

    // JSON body — how Mastodon clients send it.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id1}/reblog"),
        Some(&bob),
        Some(json!({"visibility": "unlisted"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "unlisted");

    // Query string.
    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id2}/reblog?visibility=unlisted"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["visibility"], "unlisted");

    // Form body.
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/statuses/{id1}/reblog"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("authorization", format!("Bearer {carol}"))
                .body(Body::from("visibility=unlisted"))
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
    assert_eq!(body["visibility"], "unlisted");

    // Invalid value is rejected wherever it arrives.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id2}/reblog"),
        Some(&carol),
        Some(json!({"visibility": "private"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bookmark_roundtrip_silent() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "save me", "public").await;

    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/bookmark"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["bookmarked"], true);

    let (code, body) = request(&env.app, "GET", "/api/v1/bookmarks", Some(&bob), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["id"], id);

    // No notification event — bookmarks are silent.
    let notifications: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE type = 'favourite' OR status_id = $1",
    )
    .bind(id.parse::<i64>().unwrap())
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(notifications, 0);

    let (code, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unbookmark"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["bookmarked"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quote_roundtrip() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "quote me", "public").await;

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&bob),
        Some(json!({"status": "thoughts", "quote_id": id})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let html = body["content"].as_str().unwrap();
    assert!(html.contains("RE:"), "{html}");
    assert_eq!(body["quote_id"], id);
    assert_eq!(body["quote"]["state"], "accepted");
    assert_eq!(body["quote"]["quoted_status"]["id"], id);
    let quote_status_id = body["id"].as_str().unwrap().parse::<i64>().unwrap();

    // quotes_count on the target.
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["quotes_count"], 1);

    // The emitted object carries every FEP-0449 spelling.
    let ctx = yonder::posts::PostContext::new(&env.state);
    let status = yonder::posts::load(&ctx.db, quote_status_id)
        .await
        .unwrap()
        .unwrap();
    let obj = activitypub::posts::build_object(&ctx, &status)
        .await
        .unwrap();
    assert!(
        obj.get("quote").is_some() || obj.get("quoteUri").is_some(),
        "{obj}"
    );
    assert!(obj.get("interactionPolicy").is_some());

    // reply + quote is rejected.
    let (code, _) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&bob),
        Some(json!({"status": "x", "in_reply_to_id": id, "quote_id": id})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emoji_reactions_roundtrip() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "react to me", "public").await;

    // Unicode react.
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/pleroma/statuses/{id}/reactions/%F0%9F%91%8D"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let reactions = body["pleroma"]["emoji_reactions"].as_array().unwrap();
    assert_eq!(reactions[0]["count"], 1);
    assert_eq!(reactions[0]["me"], true);

    // A second distinct emoji coexists.
    let (code, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/pleroma/statuses/{id}/reactions/%F0%9F%8E%89"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body["pleroma"]["emoji_reactions"].as_array().unwrap().len(),
        2
    );
    // Reactions are not favourites.
    assert_eq!(body["favourites_count"], 0);

    // Unknown shortcode → 422.
    let (code, _) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/pleroma/statuses/{id}/reactions/nonexistent_emoji"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Listing.
    let (code, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/pleroma/statuses/{id}/reactions"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);

    // Unreact.
    let (code, body) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/pleroma/statuses/{id}/reactions/%F0%9F%91%8D"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body["pleroma"]["emoji_reactions"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_roundtrip_and_cap() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;

    // Cap is 5 — pin 5 posts, the 6th fails.
    let mut ids = Vec::new();
    for i in 0..6 {
        let (id, _) = post(&env.app, &alice, &format!("pin {i}"), "public").await;
        ids.push(id);
    }
    for id in &ids[..5] {
        let (code, body) = request(
            &env.app,
            "POST",
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&alice),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(body["pinned"], true);
    }
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{}/pin", ids[5]),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);

    // Only the author can pin.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{}/pin", ids[5]),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    // Featured collection renders pinned object docs.
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get("/ap/users/alice/collections/featured")
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
    assert_eq!(doc["totalItems"], 5);
    assert_eq!(doc["orderedItems"].as_array().unwrap().len(), 5);

    // Unpin frees a slot.
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{}/unpin", ids[0]),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{}/pin", ids[5]),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_like_announce_emojireact() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let (id, uri) = post(&env.app, &alice, "federate me", "public").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";

    // Inbound Like → favourites row + counter.
    let (act, raw) = activity("Like", remote_actor, json!(uri));
    let disp = interactions::inbound::handle(&core, &act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::PassThrough));
    let count: i32 = sqlx::query_scalar("SELECT favourites_count FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Double Like is a no-op.
    let (act, raw) = activity("Like", remote_actor, json!(uri));
    interactions::inbound::handle(&core, &act, &raw).unwrap();
    let count: i32 = sqlx::query_scalar("SELECT favourites_count FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Undo(Like) → row + counter removed.
    let like = pubby_sdk::build_like_activity(remote_actor, &uri, None, None, None, None, None);
    let (act, raw) = activity("Undo", remote_actor, like);
    interactions::inbound::handle(&core, &act, &raw).unwrap();
    let count: i32 = sqlx::query_scalar("SELECT favourites_count FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Inbound Announce → boost row (local=false) + counter.
    let announce = pubby_sdk::build_announce_activity(
        remote_actor,
        &uri,
        None,
        None,
        Some(&format!("{remote_actor}/activities/ann1")),
        None,
        None,
    );
    let (act, raw) = activity("Announce", remote_actor, json!(uri));
    let disp = interactions::inbound::handle(&core, &act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::PassThrough));
    let boosts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE reblog_of_id = $1 AND NOT tombstone",
    )
    .bind(id.parse::<i64>().unwrap())
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(boosts, 1);
    // Undo(Announce) tombstones it.
    let (act, raw) = activity("Undo", remote_actor, announce);
    interactions::inbound::handle(&core, &act, &raw).unwrap();
    let boosts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE reblog_of_id = $1 AND NOT tombstone",
    )
    .bind(id.parse::<i64>().unwrap())
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(boosts, 0);

    // Inbound EmojiReact (handled, not pubby's).
    let (act, mut raw) = activity("EmojiReact", remote_actor, json!(uri));
    raw["content"] = json!("🎉");
    let disp = interactions::inbound::handle(&core, &act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::Handled));
    let reactions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM emoji_reactions WHERE status_id = $1")
            .bind(id.parse::<i64>().unwrap())
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(reactions, 1);
    // Undo(EmojiReact).
    let inner = json!({"type": "EmojiReact", "object": uri, "content": "🎉"});
    let (act, raw) = activity("Undo", remote_actor, inner);
    let disp = interactions::inbound::handle(&core, &act, &raw).unwrap();
    assert!(matches!(disp, pubby_sdk::ActivityDisposition::Handled));
    let reactions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM emoji_reactions WHERE status_id = $1")
            .bind(id.parse::<i64>().unwrap())
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(reactions, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_quote_materializes() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let (id, uri) = post(&env.app, &alice, "quoted locally", "public").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";

    // A remote Create quoting our status → quote_of_id + counter.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/q1",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>quoting</p>",
            "quoteUri": uri,
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    let (quote_of, count): (Option<i64>, i32) = sqlx::query_as(
        "SELECT quote_of_id, (SELECT quotes_count FROM statuses WHERE uri = $2) \
         FROM statuses WHERE uri = $1",
    )
    .bind("https://remote.test/objects/q1")
    .bind(&uri)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(quote_of.is_some());
    assert_eq!(count, 1);

    // A remote Create quoting an unknown object → quote_uri pending,
    // back-filled when the quoted post materializes.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/q2",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>pending quote</p>",
            "quoteUri": "https://remote.test/objects/target",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    let pending: Option<String> = sqlx::query_scalar(
        "SELECT quote_uri FROM statuses WHERE uri = 'https://remote.test/objects/q2'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        pending.as_deref(),
        Some("https://remote.test/objects/target")
    );

    // The quoted post arrives — the pending quote resolves.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/target",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>the target</p>",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    let resolved: Option<i64> = sqlx::query_scalar(
        "SELECT quote_of_id FROM statuses WHERE uri = 'https://remote.test/objects/q2'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(resolved.is_some());

    // The backfill bumps only the quoted target's counter — every
    // other row is untouched.
    let target_count: i32 = sqlx::query_scalar(
        "SELECT quotes_count FROM statuses WHERE uri = 'https://remote.test/objects/target'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(target_count, 1);
    let alice_count: i32 = sqlx::query_scalar("SELECT quotes_count FROM statuses WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(alice_count, 1); // still q1's — not re-bumped
    let q1_count: i32 = sqlx::query_scalar(
        "SELECT quotes_count FROM statuses WHERE uri = 'https://remote.test/objects/q1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(q1_count, 0);
}

/// A ⇄ B quote cycle (remote posts quoting each other — the second
/// back-fills the first's pending `quote_uri`) must render with one
/// nesting level, not recurse without bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quote_cycle_renders_bounded() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";

    // A quotes B before B exists → pending quote_uri.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/a",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>a</p>",
            "quoteUri": "https://remote.test/objects/b",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    // B quotes A → resolves immediately; the backfill then links A → B.
    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": "https://remote.test/objects/b",
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>b</p>",
            "quoteUri": "https://remote.test/objects/a",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;

    let ctx = yonder::posts::PostContext::new(&env.state);
    let a = yonder::posts::load_by_uri(&ctx.db, "https://remote.test/objects/a")
        .await
        .unwrap()
        .unwrap();
    let entity = yonder::posts::entity::status_entity(&ctx, &a, None)
        .await
        .unwrap();
    assert_eq!(entity["quote"]["state"], "accepted");
    let inner = &entity["quote"]["quoted_status"];
    assert_eq!(inner["uri"], "https://remote.test/objects/b");
    // B's own quote metadata is present but its quoted_status is not
    // embedded — the cycle stops at one level.
    assert_eq!(inner["quote"]["state"], "accepted");
    assert!(inner["quote"]["quoted_status"].is_null());
}

/// A scheduled quote publishes with the full quote wiring — the
/// Pleroma `RE:` link and the target's counter (the scheduled path
/// shares the immediate path's quote tail).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_quote_publishes_with_wiring() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    let (id, _) = post(&env.app, &alice, "scheduled target", "public").await;
    let when = chrono::Utc::now() + chrono::Duration::hours(1);

    let (code, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&bob),
        Some(json!({"status": "later quote", "quote_id": id, "scheduled_at": when.to_rfc3339()})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let sid: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Due now → the publish job runs the shared quote tail.
    sqlx::query(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(sid)
    .execute(&env.state.db)
    .await
    .unwrap();
    yonder::posts::scheduled::ScheduledStatusPublishJob::new(env.state.ap.clone())
        .run(
            &JobContext {
                db: env.state.db.clone(),
                redis: env.state.redis.clone(),
                config: env.state.config().clone(),
                media: env.state.media.clone(),
            },
            &json!({"scheduled_status_id": sid}),
        )
        .await
        .unwrap();

    let (html, quotes): (Option<String>, i32) = sqlx::query_as(
        "SELECT s.content_html, t.quotes_count FROM statuses s \
         JOIN statuses t ON t.id = s.quote_of_id \
         WHERE s.content = 'later quote'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let html = html.unwrap();
    assert!(html.contains("RE:"), "{html}");
    assert_eq!(quotes, 1);
}

/// A remote-target favourite publishes a `Like` (recorded in
/// `favourites.activity_uri` + the `activities` ledger) and the
/// unfavourite publishes an `Undo` wrapping exactly that Like id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_favourite_undo_like_payload() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let bob = register(&env.app, "bob", "b@x.com").await;
    let core = env.state.ap.as_ref().unwrap().core.clone();
    let remote_actor = "https://remote.test/users/eve";
    let target_uri = "https://remote.test/objects/likeable";

    let (act, raw) = activity(
        "Create",
        remote_actor,
        json!({
            "id": target_uri,
            "type": "Note",
            "attributedTo": remote_actor,
            "content": "<p>remote likeable</p>",
            "to": [PUBLIC],
            "published": "2025-01-01T00:00:00Z",
        }),
    );
    yonder::posts::remote::handle(&core, &act, &raw).await;
    let status_id: i64 = sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
        .bind(target_uri)
        .fetch_one(&env.state.db)
        .await
        .unwrap();

    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{status_id}/favourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let like_uri: String = sqlx::query_scalar(
        "SELECT activity_uri FROM favourites \
         WHERE status_id = $1 AND activity_uri IS NOT NULL",
    )
    .bind(status_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let stored: Value =
        sqlx::query_scalar("SELECT object FROM activities WHERE activity_id = $1 AND local")
            .bind(&like_uri)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(stored["type"], "Like");
    assert_eq!(stored["object"], target_uri);

    let (code, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{status_id}/unfavourite"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let undos: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activities WHERE local \
         AND object->>'type' = 'Undo' AND object->'object'->>'id' = $1",
    )
    .bind(&like_uri)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(undos, 1);
}

/// `/favourites` pages with `limit` + `Link rel="next"`; following
/// the next cursor returns the remainder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn favourites_list_pagination() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.com").await;
    let bob = register(&env.app, "bob", "b@x.com").await;
    for i in 0..3 {
        let (id, _) = post(&env.app, &alice, &format!("fav {i}"), "public").await;
        let (code, _) = request(
            &env.app,
            "POST",
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&bob),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }

    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/favourites?limit=2")
                .header("authorization", format!("Bearer {bob}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp
        .headers()
        .get("link")
        .expect("Link header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(link.contains("rel=\"next\""), "{link}");
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body.as_array().unwrap().len(), 2);

    let next = link.split('<').nth(1).unwrap().split('>').next().unwrap();
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(next)
                .header("authorization", format!("Bearer {bob}"))
                .body(Body::empty())
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
    assert_eq!(body.as_array().unwrap().len(), 1);
}
