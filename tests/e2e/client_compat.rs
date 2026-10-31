//! Epic-26 client-API compatibility tests — the Mastodon/Pleroma
//! endpoints mobile apps (Moshidon, official Mastodon) hit on every
//! session: markers, announcements, v1 filters, push subscriptions,
//! status source/history/batch, trends, notification policy,
//! featured tags, familiar followers, domain-block preview,
//! conversation mutes, suggestions.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use redis::AsyncCommands;
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{Dispatcher, JobHandler},
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";

struct Env {
    app: Router,
    #[allow(dead_code)]
    state: Arc<AppState>,
}

async fn test_env(mut config: Config) -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    for bucket in [
        &mut config.rate_limit.authentication,
        &mut config.rate_limit.account_creation,
        &mut config.rate_limit.relation,
        &mut config.rate_limit.status,
        &mut config.rate_limit.media,
        &mut config.rate_limit.timeline,
    ] {
        *bucket = RateLimit {
            limit: 10_000,
            period_seconds: 60,
            authenticated_limit: None,
        };
    }
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-cc-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();

    let dispatcher = Dispatcher::new(&redis_handle, &config.jobs);
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

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
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
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    (status, headers, body)
}

async fn post_status(app: &Router, token: &str, text: &str) -> Value {
    let (code, _, body) = request(
        app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        Some(json!({"status": text})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn markers_round_trip() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "marker", "m@x.test").await;
    let status = post_status(&env.app, &token, "hello").await;

    // Empty before first save.
    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/markers?timeline[]=home&timeline[]=notifications",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!({}));

    // Form-encoded save, like the apps send.
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post("/api/v1/markers")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(format!(
                    "home[last_read_id]={}&notifications[last_read_id]={}",
                    status["id"].as_str().unwrap(),
                    status["id"].as_str().unwrap()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/markers?timeline[]=home",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["home"]["last_read_id"], status["id"]);
    assert_eq!(body["home"]["version"], 1);
    // Unrequested timelines stay absent.
    assert!(body.get("notifications").is_none());

    // Second save bumps the version.
    let (code, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/markers",
        Some(&token),
        Some(json!({"home": {"last_read_id": status["id"].as_str().unwrap()}})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["home"]["version"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn announcements_empty_and_dismiss_404() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ann", "a@x.test").await;
    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/announcements?with_dismissed=false",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!([]));
    let (code, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/announcements/1/dismiss",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn v1_filters_crud() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "filt", "f@x.test").await;

    // Create via the v1 phrase shape.
    let (code, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/filters",
        Some(&token),
        Some(json!({
            "phrase": "spoiler",
            "context": ["home", "public"],
            "irreversible": true,
            "whole_word": true,
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["phrase"], "spoiler");
    assert_eq!(body["context"], json!(["home", "public"]));
    assert_eq!(body["irreversible"], true);
    assert_eq!(body["whole_word"], true);
    let id = body["id"].as_str().unwrap().to_string();

    // The v2 list sees the same row (keyword view).
    let (code, _, body) = request(&env.app, "GET", "/api/v2/filters", Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["keywords"][0]["keyword"], "spoiler");

    // v1 list + show + update + delete.
    let (code, _, body) = request(&env.app, "GET", "/api/v1/filters", Some(&token), None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (code, _, body) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/filters/{id}"),
        Some(&token),
        Some(json!({"phrase": "cw: spoiler"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["phrase"], "cw: spoiler");
    let (code, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/filters/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, _, body) = request(&env.app, "GET", "/api/v1/filters", Some(&token), None).await;
    assert_eq!(body, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_source_history_batch_and_mute() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "src", "s@x.test").await;
    let status = post_status(&env.app, &token, "**bold** markdown?").await;
    let id = status["id"].as_str().unwrap();

    // Source returns raw text.
    let (code, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/source"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["text"], "**bold** markdown?");

    // History starts with just the live version.
    let (code, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/history"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["content"].as_str().unwrap(), status["content"]);

    // An edit makes it two versions, oldest first.
    let (code, _, _) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{id}"),
        Some(&token),
        Some(json!({"status": "edited text"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{id}/history"),
        Some(&token),
        None,
    )
    .await;
    let versions = body.as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["content"].as_str().unwrap(), status["content"]);
    assert!(versions[1]["content"]
        .as_str()
        .unwrap()
        .contains("edited text"));

    // Batch fetch by id[].
    let (code, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses?id[]={id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["id"], id);

    // Mute → muted=true on the entity; unmute → false.
    let (code, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/mute"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["muted"], true);
    let (_, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/unmute"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["muted"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_mute_hides_notifications() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "a@x.test").await;
    let bob = register(&env.app, "bob", "b@x.test").await;
    let status = post_status(&env.app, &alice, "root of thread").await;
    let id = status["id"].as_str().unwrap();

    // Bob replies, mentioning Alice — the reply generates her
    // `mention` notification.
    let (code, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&bob),
        Some(json!({"status": "@alice reply", "in_reply_to_id": id})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    // Notifications generate via the `NotificationFanout` job — no
    // worker runs in the test env, so drain the stream manually.
    let key = env.state.redis.key("queue:default");
    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn.xrange(&key, "-", "+").await.unwrap();
    for entry in reply.ids {
        if let Some(v) = entry.map.get("job") {
            let body: String = redis::from_redis_value(v).unwrap();
            let envelope = yonder::jobs::queue::decode(&body).unwrap();
            if envelope.kind == yonder::jobs::JobKind::NotificationFanout {
                yonder::notifications::fanout::NotificationFanoutJob
                    .run(
                        &yonder::jobs::JobContext {
                            db: env.state.db.clone(),
                            redis: env.state.redis.clone(),
                            config: env.state.config().clone(),
                            media: env.state.media.clone(),
                        },
                        &envelope.payload,
                    )
                    .await
                    .unwrap();
            }
        }
        let _: i64 = conn.xdel(&key, &[entry.id]).await.unwrap();
    }
    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");

    // Muting the *root* hides the reply notification.
    request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{id}/mute"),
        Some(&alice),
        None,
    )
    .await;
    let (_, _, body) = request(&env.app, "GET", "/api/v1/notifications", Some(&alice), None).await;
    assert_eq!(body, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn trends_endpoints() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "tr", "t@x.test").await;
    let status = post_status(&env.app, &token, "trending #trendtest").await;
    // A favourite makes it trend.
    let (code, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{}/favourite", status["id"].as_str().unwrap()),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);

    for path in [
        "/api/v1/trends",
        "/api/v1/trends/statuses",
        "/api/v1/trends/links",
    ] {
        let (code, _, _) = request(&env.app, "GET", path, Some(&token), None).await;
        assert_eq!(code, StatusCode::OK, "{path}");
    }
    let (_, _, body) = request(&env.app, "GET", "/api/v1/trends/statuses", Some(&token), None).await;
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&status["id"].as_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn notification_policy_and_requests() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "pol", "p@x.test").await;

    // `policy` no longer falls into `{id}` → 400; it returns defaults.
    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["filter_not_followers"], false);
    assert_eq!(body["summary"]["pending_requests_count"], 0);

    let (code, _, body) = request(
        &env.app,
        "PUT",
        "/api/v1/notifications/policy",
        Some(&token),
        Some(json!({"filter_not_followers": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["filter_not_followers"], true);
    // Persisted.
    let (_, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/policy",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(body["filter_not_followers"], true);

    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/notifications/requests",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!([]));
    let (code, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/notifications/requests/1/accept",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn push_subscription_crud() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "push", "pu@x.test").await;
    // The `push` scope is a distinct base scope — registration issues
    // `read write`; grant it like an OAuth authorize would.
    sqlx::query("UPDATE oauth_access_tokens SET scopes = scopes || ' push'")
        .execute(&env.state.db)
        .await
        .unwrap();

    let (code, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);

    let (code, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({
            "subscription": {"endpoint": "https://push.example/sub/1", "keys": {"p256dh": "k1", "auth": "a1"}},
            "data": {"alerts": {"favourite": false}},
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["endpoint"], "https://push.example/sub/1");
    assert_eq!(body["alerts"]["favourite"], false);
    assert_eq!(body["alerts"]["mention"], true);

    // Update alerts.
    let (code, _, body) = request(
        &env.app,
        "PUT",
        "/api/v1/push/subscription",
        Some(&token),
        Some(json!({"data": {"alerts": {"favourite": true}}})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["alerts"]["favourite"], true);

    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.get("server_key").is_some());

    let (code, _, _) = request(
        &env.app,
        "DELETE",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/push/subscription",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn featured_tags_flow() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "ft", "ft@x.test").await;
    post_status(&env.app, &token, "post about #rust and #fediverse").await;

    // Suggestions surface used tags.
    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/featured_tags/suggestions",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(!body.as_array().unwrap().is_empty());

    // Feature one, read own + per-account lists.
    let (code, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/featured_tags",
        Some(&token),
        Some(json!({"name": "#rust"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "rust");
    assert_eq!(body["statuses_count"], 1);
    let ft_id = body["id"].as_str().unwrap().to_string();

    let (_, _, creds) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&token),
        None,
    )
    .await;
    let account_id = creds["id"].as_str().unwrap();
    let (code, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/{account_id}/featured_tags"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["name"], "rust");

    let (code, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/featured_tags/{ft_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_search_and_familiar_followers() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alicex", "ax@x.test").await;
    let bob = register(&env.app, "bobx", "bx@x.test").await;
    let carol = register(&env.app, "carolx", "cx@x.test").await;
    let (_, _, creds) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&carol),
        None,
    )
    .await;
    let carol_id = creds["id"].as_str().unwrap();

    // accounts/search hits local users.
    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/search?q=carolx",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["acct"], "carolx");

    // alice follows bob; bob follows carol → bob is a familiar
    // follower of carol for alice.
    let (_, _, bob_creds) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&bob),
        None,
    )
    .await;
    let bob_id = bob_creds["id"].as_str().unwrap().to_string();
    for (token, target_id) in [(&alice, &bob_id), (&bob, &carol_id.to_string())] {
        let (code, _, body) = request(
            &env.app,
            "POST",
            &format!("/api/v1/accounts/{target_id}/follow"),
            Some(token),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    let (code, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/accounts/familiar_followers?id[]={carol_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body[0]["id"], carol_id);
    assert_eq!(body[0]["accounts"][0]["acct"], "bobx");
}

#[tokio::test(flavor = "multi_thread")]
async fn domain_preview_suggestions_extended_desc() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "dp", "dp@x.test").await;

    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/domain_blocks/preview?domain=example.com",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["followingCount"], 0);

    let (code, _, body) = request(
        &env.app,
        "GET",
        "/api/v2/suggestions",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, json!([]));

    // No extended description configured → 404 (Mastodon semantics).
    let (code, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/instance/extended_description",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn media_get_v1() {
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env.app, "med", "md@x.test").await;
    // Unknown id → 404 under v1 too (route exists).
    let (code, _, _) = request(
        &env.app,
        "GET",
        "/api/v1/media/999999",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}
