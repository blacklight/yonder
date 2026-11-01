//! Webmention send/receive e2e tests — a mock remote site serves an
//! `h-entry` source page (incoming) and a page advertising a
//! Webmention endpoint that captures deliveries (outgoing). The
//! worker drains the real job handlers end to end.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::Form,
    http::{HeaderMap, Request, StatusCode},
    response::Html,
    routing::{get, post},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{Dispatcher, JobContext, Registry},
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";

struct Env {
    app: Router,
    state: Arc<AppState>,
    registry: Arc<Registry>,
}

async fn test_env() -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let mut config = Config::default();
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    for bucket in [
        &mut config.rate_limit.authentication,
        &mut config.rate_limit.account_creation,
        &mut config.rate_limit.status,
        &mut config.rate_limit.webmention,
    ] {
        *bucket = RateLimit {
            limit: 10_000,
            period_seconds: 60,
            authenticated_limit: None,
        };
    }
    config.jobs.poll_interval_ms = 50;
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-rs-{}", uuid::Uuid::new_v4()))
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
    let registry = Arc::new(yonder::cli::build_registry(state.ap.as_ref()));
    let state = Arc::new(state);
    Some(Env {
        app: build_router(state.clone()),
        state,
        registry,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    content_type: &str,
    body: impl Into<String>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", content_type);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.into())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

async fn post_status(app: &Router, token: &str, text: &str) -> String {
    let (status, _, body) = request(
        app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        "application/json",
        json!({"status": text, "visibility": "public"}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["id"].as_str().unwrap().to_string()
}

/// Spawn a worker draining all queues; caller cancels via the token.
fn start_worker(
    env: &Env,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<yonder::Result<()>>,
) {
    let job_ctx = JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config(),
        media: env.state.media.clone(),
    };
    let worker = yonder::jobs::worker::Worker::new(
        job_ctx,
        env.registry.clone(),
        Arc::new(env.state.config().jobs.clone()),
        "test-wm",
    );
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });
    (cancel, handle)
}

/// Poll `check` until it returns true or `timeout` elapses.
async fn wait_for<F, Fut>(timeout: Duration, check: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn urlenc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The captured `source`/`target` pairs a mock receiver got.
type Captured = Arc<Mutex<Vec<(String, String)>>>;

/// A mock remote site on an ephemeral loopback port.
///
/// - `GET /article` serves `article(base)` — the `h-entry` source page.
/// - `GET /target` serves a page advertising `<link rel="webmention">`.
/// - `POST /wm` is the Webmention endpoint, capturing deliveries.
///
/// Returns `(base_url, captured)`.
async fn mock_site(article: impl FnOnce(&str) -> String) -> (String, Captured) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let wm_url = format!("{base}/wm");
    let target_page = format!(
        "<html><head><link rel=\"webmention\" href=\"{wm_url}\" /></head>\
         <body><p>remote page</p></body></html>"
    );
    let article = article(&base);
    let app = Router::new()
        .route(
            "/article",
            get({
                let article = article.clone();
                move || {
                    let article = article.clone();
                    async move { Html(article) }
                }
            }),
        )
        .route(
            "/target",
            get(move || {
                let target_page = target_page.clone();
                async move { Html(target_page) }
            }),
        )
        .route(
            "/wm",
            post({
                let captured = captured.clone();
                move |Form(form): Form<HashMap<String, String>>| {
                    let captured = captured.clone();
                    async move {
                        captured
                            .lock()
                            .unwrap()
                            .push((form["source"].clone(), form["target"].clone()));
                        StatusCode::OK
                    }
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, captured)
}

fn h_entry(article_url: &str, author_url: &str, target_url: &str, wm_url: &str) -> String {
    format!(
        "<html><head><link rel=\"webmention\" href=\"{wm_url}\" /></head>\
         <body><article class=\"h-entry\">\
         <span class=\"p-author h-card\">\
           <a class=\"u-url\" href=\"{author_url}\"><span class=\"p-name\">Site Author</span></a>\
         </span>\
         <div class=\"e-content\"><p>Great post — \
           <a href=\"{target_url}\">read the original</a></p></div>\
         <a class=\"u-url\" href=\"{article_url}\">permalink</a>\
         <a class=\"u-in-reply-to\" href=\"{target_url}\">in reply to</a>\
         </article></body></html>"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An incoming Webmention is verified and materializes as a reply by
/// a pseudo-actor; reposts are idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incoming_webmention_materializes_reply() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let token = register(&env.app, "wmrecv", "wmrecv@example.com").await;
    let status_id = post_status(&env.app, &token, "a local post worth mentioning").await;
    let target = format!("{BASE}/@wmrecv/{status_id}");
    let tgt = target.clone();
    let (site, _) = mock_site(move |base| {
        h_entry(
            &format!("{base}/article"),
            &format!("{base}/author"),
            &tgt,
            &format!("{base}/wm"),
        )
    })
    .await;
    let source = format!("{site}/article");

    // Receive the mention — the endpoint only queues.
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        format!("source={}&target={}", urlenc(&source), urlenc(&target)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (cancel, worker) = start_worker(&env);
    let db = env.state.db.clone();
    let src = source.clone();
    let tgt2 = target.clone();
    let materialized = wait_for(Duration::from_secs(20), move || {
        let db = db.clone();
        let src = src.clone();
        let tgt = tgt2.clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT status_id FROM webmentions \
                 WHERE source = $1 AND target = $2 AND direction = 'incoming'",
            )
            .bind(&src)
            .bind(&tgt)
            .fetch_optional(&db)
            .await
            .unwrap()
            .flatten()
            .is_some()
        }
    })
    .await;
    if !materialized {
        let rows: Vec<(String, String, String, Option<i64>)> =
            sqlx::query_as("SELECT source, target, status, status_id FROM webmentions")
                .fetch_all(&env.state.db)
                .await
                .unwrap();
        panic!("webmention reply was not materialized; ledger rows: {rows:?}");
    }

    // The `mention` notification goes through the NotificationFanout
    // job — it lands while the worker is still running.
    let db = env.state.db.clone();
    let notified = wait_for(Duration::from_secs(10), move || {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM notifications n \
                 JOIN statuses s ON s.id = n.status_id \
                 WHERE n.type = 'mention' AND s.uri LIKE 'urn:yonder:webmention:%')",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        }
    })
    .await;
    assert!(notified, "mention notification not emitted");
    cancel.cancel();
    worker.await.unwrap().unwrap();

    // The reply row.
    let (local, in_reply_to, uri, url, visibility): (
        bool,
        Option<i64>,
        String,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "SELECT local, in_reply_to_id, uri, url, visibility FROM statuses \
             WHERE uri LIKE 'urn:yonder:webmention:status:%'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!local);
    assert_eq!(in_reply_to, Some(status_id.parse::<i64>().unwrap()));
    assert!(uri.starts_with("urn:yonder:webmention:status:"), "{uri}");
    assert_eq!(url.as_deref(), Some(source.as_str()));
    assert_eq!(visibility, "public");

    // The pseudo-actor.
    let (domain, actor_id, acct_name, discoverable): (String, String, String, bool) =
        sqlx::query_as(
            "SELECT a.domain, a.actor_id, a.username, a.discoverable \
             FROM accounts a JOIN statuses s ON s.account_id = a.id \
             WHERE s.uri = $1",
        )
        .bind(&uri)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let site_host = site.trim_start_matches("http://");
    assert_eq!(domain, site_host);
    assert!(actor_id.starts_with("urn:yonder:webmention:"), "{actor_id}");
    assert_eq!(acct_name, "author");
    assert!(!discoverable);

    // Parent counter and the reply in the local thread.
    let replies: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1")
        .bind(status_id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(replies, 1);
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{status_id}/context"),
        Some(&token),
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    let descendants = body["descendants"].as_array().unwrap();
    assert_eq!(descendants.len(), 1, "{body}");
    assert!(descendants[0]["account"]["acct"]
        .as_str()
        .unwrap()
        .ends_with(&format!("@{site_host}")));

    // Idempotent: re-sending the same mention updates the ledger row
    // (updated_at advances) but never duplicates the reply.
    let first_updated: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT updated_at FROM webmentions WHERE source = $1 AND direction = 'incoming'",
    )
    .bind(&source)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        format!("source={}&target={}", urlenc(&source), urlenc(&target)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (cancel, worker) = start_worker(&env);
    let db = env.state.db.clone();
    let src = source.clone();
    let reprocessed = wait_for(Duration::from_secs(20), move || {
        let db = db.clone();
        let src = src.clone();
        async move {
            sqlx::query_scalar::<_, chrono::DateTime<chrono::Utc>>(
                "SELECT updated_at FROM webmentions WHERE source = $1 AND direction = 'incoming'",
            )
            .bind(&src)
            .fetch_one(&db)
            .await
            .map(|ts| ts > first_updated)
            .unwrap_or(false)
        }
    })
    .await;
    cancel.cancel();
    worker.await.unwrap().unwrap();
    assert!(reprocessed, "re-sent webmention was not re-verified");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE uri LIKE 'urn:yonder:webmention:status:%' \
         AND NOT tombstone",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let replies: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1")
        .bind(status_id.parse::<i64>().unwrap())
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(replies, 1);
}

/// A local public post's links are delivered to discovered
/// Webmention endpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outgoing_webmention_delivers() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let token = register(&env.app, "wmsend", "wmsend@example.com").await;

    let (site, captured) = mock_site(|_| String::new()).await;
    let target_page = format!("{site}/target");
    let status_id = post_status(&env.app, &token, &format!("check this out {target_page}")).await;

    let (cancel, worker) = start_worker(&env);
    let delivered = wait_for(Duration::from_secs(20), {
        let captured = captured.clone();
        move || {
            let captured = captured.clone();
            async move { !captured.lock().unwrap().is_empty() }
        }
    })
    .await;
    assert!(delivered, "webmention was not delivered");
    cancel.cancel();
    worker.await.unwrap().unwrap();

    let sent = captured.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1, target_page);
    assert_eq!(sent[0].0, format!("{BASE}/webmentions/source/{status_id}"));

    // The outgoing ledger row was recorded.
    let stored: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM webmentions WHERE source = $1 \
         AND target = $2 AND direction = 'outgoing' AND status = 'confirmed')",
    )
    .bind(&sent[0].0)
    .bind(&target_page)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(stored);

    // The h-entry source page serves the mention markup.
    let (status, headers, body) = request(
        &env.app,
        "GET",
        &format!("/webmentions/source/{status_id}"),
        None,
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("class=\"h-entry\""), "{html}");
    assert!(html.contains(&target_page), "{html}");
    let link = headers
        .get(axum::http::header::LINK)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(link.contains("rel=\"webmention\""), "{link}");
}

/// Edge validation on `POST /webmentions` and the endpoint
/// advertisement on HTML responses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webmention_guards() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };

    // Missing fields → 400.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        "source=https://example.com/a",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Foreign target → 400.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        "source=https://example.com/a&target=https://other.example/b",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Suspended source domain → 403.
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('spam.example', 'suspend')")
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        format!(
            "source=https://spam.example/a&target={}/@nobody/1",
            urlenc(BASE)
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Every text/html response advertises the endpoint — how remote
    // senders discover the receiver from our pages.
    let (_, headers, _) = request(&env.app, "GET", "/", None, "text/html", "").await;
    let link = headers
        .get(axum::http::header::LINK)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        link.contains(&format!("<{BASE}/webmentions>; rel=\"webmention\"")),
        "Link: {link}"
    );
}

/// Interactions on a webmention-materialized reply backfeed to the
/// source site: a reply delivers `in-reply-to`, a favourite delivers
/// `like-of`, and an unfavourite retracts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interaction_backfeed() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let token = register(&env.app, "wmback", "wmback@example.com").await;
    let (status, _, body) = request(
        &env.app,
        "GET",
        "/api/v1/accounts/verify_credentials",
        Some(&token),
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let me: Value = serde_json::from_slice(&body).unwrap();
    let my_account_id: i64 = me["id"].as_str().unwrap().parse().unwrap();

    let status_id = post_status(&env.app, &token, "a local post").await;
    let target = format!("{BASE}/@wmback/{status_id}");
    let tgt = target.clone();
    let (site, captured) = mock_site(move |base| {
        h_entry(
            &format!("{base}/article"),
            &format!("{base}/author"),
            &tgt,
            &format!("{base}/wm"),
        )
    })
    .await;
    let source = format!("{site}/article");

    // Materialize the incoming mention → the wm-reply exists.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        format!("source={}&target={}", urlenc(&source), urlenc(&target)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (cancel, worker) = start_worker(&env);
    let db = env.state.db.clone();
    let src = source.clone();
    let wm_reply_id = wait_for(Duration::from_secs(20), move || {
        let db = db.clone();
        let src = src.clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT status_id FROM webmentions WHERE source = $1 AND direction = 'incoming'",
            )
            .bind(&src)
            .fetch_optional(&db)
            .await
            .unwrap()
            .flatten()
            .is_some()
        }
    })
    .await;
    assert!(wm_reply_id);
    let wm_reply_id: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT status_id FROM webmentions WHERE source = $1 AND direction = 'incoming'",
    )
    .bind(&source)
    .fetch_one(&env.state.db)
    .await
    .unwrap()
    .unwrap();

    // Replying to the wm-reply backfeeds an `in-reply-to` mention to
    // the source post — even with no URL in the reply text.
    let reply_id = post_status_ext(
        &env.app,
        &token,
        &json!({"status": "replying back", "in_reply_to_id": wm_reply_id.to_string()}),
    )
    .await;
    let db = env.state.db.clone();
    let delivered = wait_for(Duration::from_secs(20), {
        let captured = captured.clone();
        let source = source.clone();
        move || {
            let captured = captured.clone();
            let source = source.clone();
            let _ = &db;
            async move { captured.lock().unwrap().iter().any(|(_, t)| *t == source) }
        }
    })
    .await;
    assert!(delivered, "reply webmention not delivered");
    let sent = captured.lock().unwrap().clone();
    let reply_mention = sent
        .iter()
        .find(|(s, t)| s.ends_with(&format!("/{reply_id}")) && *t == source)
        .expect("reply mention missing");
    assert!(reply_mention.0.contains("/webmentions/source/"));

    // The reply's h-entry marks the parent link as u-in-reply-to.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/webmentions/source/{reply_id}"),
        None,
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("u-in-reply-to"), "{html}");

    // Liking the wm-reply backfeeds a `like-of` mention.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{wm_reply_id}/favourite"),
        Some(&token),
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let like_source = format!("{BASE}/webmentions/like/{wm_reply_id}/{my_account_id}");
    let delivered = wait_for(Duration::from_secs(20), {
        let captured = captured.clone();
        let like_source = like_source.clone();
        move || {
            let captured = captured.clone();
            let like_source = like_source.clone();
            async move {
                captured
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(s, _)| *s == like_source)
            }
        }
    })
    .await;
    assert!(delivered, "like webmention not delivered");
    let sent = captured.lock().unwrap().clone();
    let like_mention = sent
        .iter()
        .find(|(s, t)| *s == like_source && *t == source)
        .expect("like mention missing");
    assert_eq!(like_mention.1, source);

    // The like source page renders the u-like-of link.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/webmentions/like/{wm_reply_id}/{my_account_id}"),
        None,
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("u-like-of"), "{html}");
    assert!(html.contains(&source), "{html}");

    // Unfavouriting retracts — the receiver is re-notified and the
    // ledger row is marked deleted.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{wm_reply_id}/unfavourite"),
        Some(&token),
        "application/json",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let db = env.state.db.clone();
    let like_source2 = like_source.clone();
    let retracted = wait_for(Duration::from_secs(20), move || {
        let db = db.clone();
        let like_source = like_source2.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM webmentions WHERE source = $1 \
                 AND direction = 'outgoing' AND status = 'deleted')",
            )
            .bind(&like_source)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    })
    .await;
    assert!(retracted, "like webmention not retracted");
    cancel.cancel();
    worker.await.unwrap().unwrap();
}

/// `POST /api/v1/statuses` with a full payload.
async fn post_status_ext(app: &Router, token: &str, payload: &Value) -> String {
    let (status, _, body) = request(
        app,
        "POST",
        "/api/v1/statuses",
        Some(token),
        "application/json",
        payload.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["id"].as_str().unwrap().to_string()
}

/// A mention whose target is the standalone embed page resolves to the
/// same status — the embed URL form (`/statuses/{id}/embed`, the
/// `<iframe>`/`<script>` target) folds under the post it renders.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incoming_webmention_to_embed_url_materializes_reply() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let token = register(&env.app, "wmembed", "wmembed@example.com").await;
    let status_id = post_status(&env.app, &token, "an embedded post").await;
    let target = format!("{BASE}/statuses/{status_id}/embed");
    let tgt = target.clone();
    let (site, _) = mock_site(move |base| {
        h_entry(
            &format!("{base}/article"),
            &format!("{base}/author"),
            &tgt,
            &format!("{base}/wm"),
        )
    })
    .await;
    let source = format!("{site}/article");

    let (status, _, body) = request(
        &env.app,
        "POST",
        "/webmentions",
        None,
        "application/x-www-form-urlencoded",
        format!("source={}&target={}", urlenc(&source), urlenc(&target)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );

    let (cancel, worker) = start_worker(&env);
    let db = env.state.db.clone();
    let src = source.clone();
    let tgt2 = target.clone();
    let materialized = wait_for(Duration::from_secs(20), move || {
        let db = db.clone();
        let src = src.clone();
        let tgt = tgt2.clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT status_id FROM webmentions \
                 WHERE source = $1 AND target = $2 AND direction = 'incoming'",
            )
            .bind(&src)
            .bind(&tgt)
            .fetch_optional(&db)
            .await
            .unwrap()
            .flatten()
            .is_some()
        }
    })
    .await;
    if !materialized {
        let rows: Vec<(String, String, String, Option<i64>)> =
            sqlx::query_as("SELECT source, target, status, status_id FROM webmentions")
                .fetch_all(&env.state.db)
                .await
                .unwrap();
        panic!("embed-target webmention not materialized; ledger: {rows:?}");
    }

    // The reply hangs off the same status as a permalink-target mention.
    let reply_parent: i64 = sqlx::query_scalar(
        "SELECT in_reply_to_id FROM statuses WHERE uri LIKE 'urn:yonder:webmention:%'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(reply_parent.to_string(), status_id);

    cancel.cancel();
    worker.await.unwrap().unwrap();
}
