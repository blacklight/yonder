//! Remote outbox sync ("sync posts from remote") e2e tests — a mock
//! remote AP server on loopback serves an actor doc and a paged
//! outbox; `POST /api/v1/accounts/{id}/sync_remote` must walk it and
//! materialize the activities through the normal ingest path.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::Dispatcher,
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";

struct Env {
    app: Router,
    state: Arc<AppState>,
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
        &mut config.rate_limit.relation,
        &mut config.rate_limit.status,
        &mut config.rate_limit.timeline,
        &mut config.rate_limit.remote_sync,
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
    let state = Arc::new(state);
    Some(Env {
        app: build_router(state.clone()),
        state,
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
            req.body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    (status, headers, body)
}

/// A mock remote instance: serves the JSON documents `docs` maps
/// (`/actor`, `/outbox`, …) on an ephemeral loopback port for the
/// lifetime of the test. Returns the base URL (`http://127.0.0.1:…`).
async fn serve_docs(build: impl FnOnce(&str) -> Vec<(&'static str, Value)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = Router::new();
    for (path, doc) in build(&base) {
        app = app.route(
            path,
            get(move || {
                let doc = doc.clone();
                async move { Json(doc) }
            }),
        );
    }
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn actor_doc(base: &str, name: &str) -> Value {
    json!({
        "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
        "id": format!("{base}/actor"),
        "type": "Person",
        "preferredUsername": name,
        "name": "Remote User",
        "inbox": format!("{base}/inbox"),
        "outbox": format!("{base}/outbox"),
        "followers": format!("{base}/followers"),
        "publicKey": {
            "id": format!("{base}/actor#main-key"),
            "owner": format!("{base}/actor"),
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtest\n-----END PUBLIC KEY-----",
        },
    })
}

fn create(base: &str, n: u32, published: &str) -> Value {
    json!({
        "id": format!("{base}/activities/create-{n}"),
        "type": "Create",
        "actor": format!("{base}/actor"),
        "published": published,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{base}/objects/{n}"),
            "type": "Note",
            "attributedTo": format!("{base}/actor"),
            "content": format!("<p>remote post {n}</p>"),
            "published": published,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        },
    })
}

/// The outbox document set: 5 importable items over two pages (three
/// Creates + one Announce on p1, one Create + one foreign-actor item +
/// one bare Note on p2), plus the announced object and its author.
fn remote_docs(base: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("/actor", actor_doc(base, "remoteuser")),
        ("/actor2", actor_doc(base, "otheruser")),
        (
            "/outbox",
            json!({
                "id": format!("{base}/outbox"),
                "type": "OrderedCollection",
                "totalItems": 7,
                "first": format!("{base}/outbox/p1"),
            }),
        ),
        (
            "/outbox/p1",
            json!({
                "id": format!("{base}/outbox/p1"),
                "type": "OrderedCollectionPage",
                "partOf": format!("{base}/outbox"),
                "next": format!("{base}/outbox/p2"),
                "orderedItems": [
                    create(base, 1, "2026-10-30T10:00:00Z"),
                    create(base, 2, "2026-10-29T10:00:00Z"),
                    {
                        "id": format!("{base}/activities/announce-1"),
                        "type": "Announce",
                        "actor": format!("{base}/actor"),
                        "published": "2026-10-28T10:00:00Z",
                        "object": format!("{base}/objects/boosted"),
                    },
                    {
                        "id": format!("{base}/activities/like-1"),
                        "type": "Like",
                        "actor": format!("{base}/actor"),
                        "published": "2026-10-27T10:00:00Z",
                        "object": format!("{base}/objects/1"),
                    },
                ],
            }),
        ),
        (
            "/outbox/p2",
            json!({
                "id": format!("{base}/outbox/p2"),
                "type": "OrderedCollectionPage",
                "partOf": format!("{base}/outbox"),
                "orderedItems": [
                    create(base, 3, "2026-10-26T10:00:00Z"),
                    // Not the actor's own activity — must be skipped.
                    {
                        "id": format!("{base}/activities/create-x"),
                        "type": "Create",
                        "actor": format!("{base}/actor2"),
                        "published": "2026-10-25T10:00:00Z",
                        "object": {
                            "id": format!("{base}/objects/x"),
                            "type": "Note",
                            "attributedTo": format!("{base}/actor2"),
                            "content": "not ours",
                            "published": "2026-10-25T10:00:00Z",
                        },
                    },
                    // A bare object (no Create wrapper) — ingested via
                    // the synthetic-Create path.
                    {
                        "id": format!("{base}/objects/4"),
                        "type": "Note",
                        "attributedTo": format!("{base}/actor"),
                        "content": "bare note",
                        "published": "2026-10-24T10:00:00Z",
                        "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    },
                ],
            }),
        ),
        (
            "/objects/boosted",
            json!({
                "id": format!("{base}/objects/boosted"),
                "type": "Note",
                "attributedTo": format!("{base}/actor2"),
                "content": "boosted post",
                "published": "2026-10-20T10:00:00Z",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            }),
        ),
    ]
}

/// Seed the remote account row (the bare-stub shape profile reads
/// produce) and return its local account id.
async fn seed_remote_account(db: &PgPool, base: &str) -> i64 {
    let actor = format!("{base}/actor");
    let domain = base.trim_start_matches("http://").to_string();
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'')",
    )
    .bind(&domain)
    .bind("remoteuser")
    .bind(&actor)
    .execute(db)
    .await
    .unwrap();
    sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(&actor)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn status_id_by_uri(db: &PgPool, uri: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1 AND NOT tombstone")
        .bind(uri)
        .fetch_optional(db)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_remote_backfills_outbox_pages() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(remote_docs).await;
    let remote_id = seed_remote_account(&env.state.db, &base).await;
    let token = register(&env.app, "syncer", "syncer@example.com").await;

    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // 3 Creates + 1 bare Note + 1 Announce = 5 imported; the Like and
    // the foreign-actor Create are scanned but skipped.
    assert_eq!(body["imported"].as_i64().unwrap(), 5, "{body}");
    assert_eq!(body["pages"].as_i64().unwrap(), 2, "{body}");
    assert_eq!(body["complete"].as_bool().unwrap(), true, "{body}");

    // The statuses materialized under the remote account.
    for n in 1..=4u32 {
        let id = status_id_by_uri(&env.state.db, &format!("{base}/objects/{n}")).await;
        assert!(id.is_some(), "objects/{n} missing");
    }
    // The Announce produced a boost row of the fetched remote object.
    let boosted = status_id_by_uri(&env.state.db, &format!("{base}/objects/boosted"))
        .await
        .expect("announced object materialized");
    let boost: Option<(i64, i64)> = sqlx::query_as(
        "SELECT id, reblog_of_id FROM statuses \
         WHERE account_id = $1 AND reblog_of_id = $2 AND NOT tombstone",
    )
    .bind(remote_id)
    .bind(boosted)
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    assert!(boost.is_some(), "boost row missing");

    // A second unbounded sync hits the stored floor — nothing new to
    // import, and it stops almost immediately.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"].as_i64().unwrap(), 0, "{body}");
    assert_eq!(body["complete"].as_bool().unwrap(), true, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_remote_limit_and_bounds() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(remote_docs).await;
    let remote_id = seed_remote_account(&env.state.db, &base).await;
    let token = register(&env.app, "bounder", "bounder@example.com").await;

    // limit=2 → the two newest Creates only; the window isn't done.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        Some(json!({"limit": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"].as_i64().unwrap(), 2, "{body}");
    assert_eq!(body["complete"].as_bool().unwrap(), false, "{body}");

    // max_id = objects/2 → only strictly-older items import.
    let anchor = status_id_by_uri(&env.state.db, &format!("{base}/objects/2"))
        .await
        .unwrap();
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        Some(json!({"max_id": anchor.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Older than objects/2: the Announce + objects/3 + bare Note = 3
    // (the foreign Create and the Like don't count).
    assert_eq!(body["imported"].as_i64().unwrap(), 3, "{body}");

    // min_id = objects/2 → only strictly-newer items: objects/1, and
    // it's already materialized → 0 new imports.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        Some(json!({"min_id": anchor.to_string()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"].as_i64().unwrap(), 0, "{body}");

    // Invalid and foreign-account bound references.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        Some(json!({"limit": "abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let local_id = account_id(&env, "bounder").await;
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        Some(json!({"min_id": "999999"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = local_id;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_remote_guards() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let token = register(&env.app, "guarded", "guarded@example.com").await;
    let local_id = account_id(&env, "guarded").await;

    // Local accounts can't be synced.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{local_id}/sync_remote"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Missing account → 404; missing token → 401.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/accounts/999999/sync_remote",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{local_id}/sync_remote"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A suspended remote domain is refused outright.
    let base = serve_docs(remote_docs).await;
    let remote_id = seed_remote_account(&env.state.db, &base).await;
    let domain = base.trim_start_matches("http://").to_string();
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ($1, 'suspend')")
        .bind(&domain)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/accounts/{remote_id}/sync_remote"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn account_id(env: &Env, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}
