//! Remote thread-context fetch ("lazy thread expansion") e2e tests —
//! a mock remote AP server on loopback serves status objects with
//! `inReplyTo` chains and `replies` collections;
//! `POST /api/v1/statuses/{id}/fetch_remote_context` must walk them
//! and materialize the thread through the normal ingest path.
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
        &mut config.rate_limit.remote_context,
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
/// (`/objects/leaf`, `/replies/leaf`, …) on an ephemeral loopback
/// port for the lifetime of the test. Returns the base URL.
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
        "id": format!("{base}/{name}"),
        "type": "Person",
        "preferredUsername": name,
        "name": "Remote User",
        "inbox": format!("{base}/inbox"),
        "outbox": format!("{base}/outbox"),
        "followers": format!("{base}/followers"),
        "publicKey": {
            "id": format!("{base}/{name}#main-key"),
            "owner": format!("{base}/{name}"),
            "publicKeyPem": "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtest\n-----END PUBLIC KEY-----",
        },
    })
}

/// A `Note` served by the mock remote. `reply_to`/`replies` shape the
/// thread; `author` picks the actor doc it was authored by.
fn note(base: &str, name: &str, author: &str, reply_to: Option<&str>, replies: Value) -> Value {
    let mut doc = json!({
        "id": format!("{base}/objects/{name}"),
        "type": "Note",
        "attributedTo": format!("{base}/{author}"),
        "content": format!("<p>{name} content</p>"),
        "published": "2026-10-30T10:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(parent) = reply_to {
        doc["inReplyTo"] = json!(format!("{base}/objects/{parent}"));
    }
    if !replies.is_null() {
        doc["replies"] = replies;
    }
    doc
}

/// The thread served by the mock remote:
///
/// ```text
/// root ── replies → [parent]          (bare-URL collection)
///  └─ parent ── replies → [leaf]      (inline items on `replies`)
///      └─ leaf ── replies → [r1, r2]  (Collection.first inline page:
///                                    r1 a bare URI, r2 an inline
///                                    cross-author object)
///          └─ r1 ── replies → [r3]    (linked collection doc)
/// ```
///
/// `/objects/leaf` is pre-seeded locally with a NULL `in_reply_to_id`
/// — the broken state the fetch repairs.
fn remote_docs(base: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("/actor", actor_doc(base, "actor")),
        ("/actor2", actor_doc(base, "actor2")),
        (
            "/objects/root",
            note(
                base,
                "root",
                "actor",
                None,
                json!(format!("{base}/replies/root")),
            ),
        ),
        (
            "/objects/parent",
            note(
                base,
                "parent",
                "actor",
                Some("root"),
                // Inline `items` right on the replies object.
                json!({"items": [format!("{base}/objects/leaf")]}),
            ),
        ),
        (
            "/objects/leaf",
            note(
                base,
                "leaf",
                "actor",
                Some("parent"),
                // `Collection.first` → inline page: one bare URI item
                // and one inline cross-author object.
                json!({
                    "id": format!("{base}/replies/leaf"),
                    "type": "Collection",
                    "first": {
                        "type": "CollectionPage",
                        "items": [
                            format!("{base}/objects/r1"),
                            {
                                "id": format!("{base}/objects/r2"),
                                "type": "Note",
                                "attributedTo": format!("{base}/actor2"),
                                "content": "<p>r2 content</p>",
                                "published": "2026-10-30T11:00:00Z",
                                "inReplyTo": format!("{base}/objects/leaf"),
                                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                            },
                        ],
                    },
                }),
            ),
        ),
        (
            "/objects/r1",
            note(
                base,
                "r1",
                "actor",
                Some("leaf"),
                json!(format!("{base}/replies/r1")),
            ),
        ),
        (
            "/objects/r3",
            note(base, "r3", "actor2", Some("r1"), Value::Null),
        ),
        (
            "/replies/root",
            json!({
                "id": format!("{base}/replies/root"),
                "type": "OrderedCollection",
                "items": [format!("{base}/objects/parent")],
            }),
        ),
        (
            "/replies/r1",
            json!({
                "id": format!("{base}/replies/r1"),
                "type": "OrderedCollection",
                "items": [format!("{base}/objects/r3")],
            }),
        ),
    ]
}

/// Seed the remote account row and a `/objects/leaf` status row with
/// a NULL `in_reply_to_id` — how a remote reply sits locally before
/// its thread is expanded. Returns the local status id.
async fn seed_remote_leaf(db: &PgPool, base: &str) -> i64 {
    let actor = format!("{base}/actor");
    let domain = base.trim_start_matches("http://").to_string();
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'')",
    )
    .bind(&domain)
    .bind("actor")
    .bind(&actor)
    .execute(db)
    .await
    .unwrap();
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(&actor)
        .fetch_one(db)
        .await
        .unwrap();
    let uri = format!("{base}/objects/leaf");
    sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, uri, url, created_at) \
         VALUES ($1,'remote leaf','<p>remote leaf</p>','public',false,$2,$2,now()) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&uri)
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
async fn fetch_remote_context_expands_thread() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(remote_docs).await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    let token = register(&env.app, "expander", "expander@example.com").await;

    // Before: the leaf is orphaned — no ancestors, no replies.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ancestors"].as_array().unwrap().len(), 0, "{body}");
    assert_eq!(body["descendants"].as_array().unwrap().len(), 0, "{body}");

    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{leaf_id}/fetch_remote_context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // parent + root (ancestors) + r1 + r2 (leaf replies) + r3 (r1's
    // reply) = 5 new statuses; the already-local items in the
    // collections are scanned but skipped.
    assert_eq!(body["imported"].as_i64().unwrap(), 5, "{body}");
    assert!(body["complete"].as_bool().unwrap(), "{body}");

    // The chain is repaired end to end.
    let leaf: Option<i64> = sqlx::query_scalar("SELECT in_reply_to_id FROM statuses WHERE id = $1")
        .bind(leaf_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let parent = status_id_by_uri(&env.state.db, &format!("{base}/objects/parent"))
        .await
        .expect("parent materialized");
    assert_eq!(leaf, Some(parent), "leaf's parent link not repaired");

    // The context now renders the whole remote thread.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ancestors = body["ancestors"].as_array().unwrap();
    let descendants = body["descendants"].as_array().unwrap();
    assert_eq!(ancestors.len(), 2, "{body}"); // root, parent
    assert_eq!(descendants.len(), 3, "{body}"); // r1, r2, r3

    // Idempotent: a second fetch finds everything already local.
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{leaf_id}/fetch_remote_context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"].as_i64().unwrap(), 0, "{body}");
    assert!(body["complete"].as_bool().unwrap(), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_remote_context_guards() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(remote_docs).await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    let token = register(&env.app, "guardian", "guardian@example.com").await;

    // Missing token → 401.
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{leaf_id}/fetch_remote_context"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Unknown status → 404.
    let (status, _, _) = request(
        &env.app,
        "POST",
        "/api/v1/statuses/999999/fetch_remote_context",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A local status → 422 (the context is already complete).
    let (status, _, body) = request(
        &env.app,
        "POST",
        "/api/v1/statuses",
        Some(&token),
        Some(json!({"status": "local post", "visibility": "public"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let local_id = body["id"].as_str().unwrap().to_string();
    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{local_id}/fetch_remote_context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A suspended author instance → 403, no remote requests at all.
    let domain = base.trim_start_matches("http://").to_string();
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ($1, 'suspend')")
        .bind(&domain)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _, _) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{leaf_id}/fetch_remote_context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
