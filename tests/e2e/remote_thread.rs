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

/// Seed the remote account row and a `/objects/{name}` status row
/// with a NULL `in_reply_to_id` — how a remote status sits locally
/// before its thread is expanded. Returns the local status id.
async fn seed_remote_status(db: &PgPool, base: &str, name: &str) -> i64 {
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
    let uri = format!("{base}/objects/{name}");
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

/// Seed `/objects/leaf` — the remote reply the shared `remote_docs`
/// thread is anchored on.
async fn seed_remote_leaf(db: &PgPool, base: &str) -> i64 {
    seed_remote_status(db, base, "leaf").await
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

    // Before: the leaf is orphaned — `in_reply_to_id` is NULL. (A
    // plain `GET /context` would itself expand the thread now, so the
    // orphan state is only visible in the stored row.)
    let orphan: Option<i64> =
        sqlx::query_scalar("SELECT in_reply_to_id FROM statuses WHERE id = $1")
            .bind(leaf_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(orphan.is_none(), "leaf starts unlinked");

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

/// Akkoma/Pleroma shape: `replies` is a bare collection ref — an
/// `{id, type, totalItems}` object with no `first` and no inline
/// items (and a stale `totalItems: 0` to boot, matching the real
/// Akkoma rendering). The collection's own `id` is the fetch URL;
/// the served collection root yields `first`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_remote_context_expands_bare_replies_ref() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(|base| {
        vec![
            ("/actor", actor_doc(base, "actor")),
            (
                "/objects/root",
                note(
                    base,
                    "root",
                    "actor",
                    None,
                    json!({
                        "id": format!("{base}/replies/root"),
                        "type": "OrderedCollection",
                        "totalItems": 0,
                    }),
                ),
            ),
            (
                "/objects/r1",
                note(base, "r1", "actor", Some("root"), Value::Null),
            ),
            (
                "/objects/r2",
                note(base, "r2", "actor", Some("root"), Value::Null),
            ),
            (
                "/replies/root",
                json!({
                    "id": format!("{base}/replies/root"),
                    "type": "OrderedCollection",
                    "first": {
                        "type": "OrderedCollectionPage",
                        "orderedItems": [
                            format!("{base}/objects/r1"),
                            format!("{base}/objects/r2"),
                        ],
                    },
                }),
            ),
        ]
    })
    .await;
    let root_id = seed_remote_status(&env.state.db, &base, "root").await;
    let token = register(&env.app, "akkomareader", "akkomareader@example.com").await;

    let (status, _, body) = request(
        &env.app,
        "POST",
        &format!("/api/v1/statuses/{root_id}/fetch_remote_context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["imported"].as_i64().unwrap(), 2, "{body}");
    assert!(body["complete"].as_bool().unwrap(), "{body}");

    // Both replies materialized and linked under the root.
    for name in ["r1", "r2"] {
        let reply_to: Option<i64> = sqlx::query_scalar(
            "SELECT in_reply_to_id FROM statuses WHERE uri = $1 AND NOT tombstone",
        )
        .bind(format!("{base}/objects/{name}"))
        .fetch_one(&env.state.db)
        .await
        .unwrap_or_else(|_| panic!("{name} should materialize"));
        assert_eq!(reply_to, Some(root_id), "{name} should link to root");
    }
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

/// Request-counting variant of [`serve_docs`] — the grace-period
/// assertion needs to prove the second `/context` read makes *no*
/// remote calls.
async fn serve_docs_counted(
    build: impl FnOnce(&str) -> Vec<(&'static str, Value)>,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = Router::new();
    for (path, doc) in build(&base) {
        let counter = counter.clone();
        app = app.route(
            path,
            get(move || {
                let doc = doc.clone();
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(doc)
                }
            }),
        );
    }
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, counter)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_context_expands_remote_status_and_grace_suppresses_refetch() {
    let Some(env) = test_env().await else { return };
    let (base, counter) = serve_docs_counted(remote_docs).await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    let token = register(&env.app, "viewer", "viewer@test.local").await;

    // A plain GET /context — no Yonder-specific call — expands the
    // remote thread inline: ancestors materialize, first-level
    // replies land, counters/expansion state get recorded.
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
    assert!(
        ancestors.len() >= 2,
        "root + parent should materialize: {body}"
    );
    assert!(
        !descendants.is_empty(),
        "replies should materialize: {body}"
    );
    let fetches_after_first = counter.load(std::sync::atomic::Ordering::SeqCst);
    assert!(fetches_after_first > 0, "expansion should fetch remotely");

    // The expansion row and the pruning recency marker were written.
    assert!(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT status_id FROM status_expansions WHERE status_id = $1"
    )
    .bind(leaf_id)
    .fetch_optional(&env.state.db)
    .await
    .unwrap()
    .is_some());
    assert!(
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT last_selected_at FROM statuses WHERE id = $1"
        )
        .bind(leaf_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
        .is_some()
    );

    // Within the grace window the same read issues no remote calls.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        fetches_after_first,
        "grace-period read must not fetch remotely"
    );
    assert_eq!(body["ancestors"].as_array().unwrap().len(), ancestors.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_parent_arrival_relinks_orphan_reply() {
    let Some(env) = test_env().await else { return };
    let base = serve_docs(remote_docs).await;
    let token = register(&env.app, "relinker", "relinker@test.local").await;

    // Seed a remote reply whose parent (`leaf`) is not stored yet —
    // `in_reply_to_uri` parked, `in_reply_to_id` NULL.
    let actor = format!("{base}/actor");
    let domain = base.trim_start_matches("http://").to_string();
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'')",
    )
    .bind(&domain)
    .bind("actor")
    .bind(&actor)
    .execute(&env.state.db)
    .await
    .unwrap();
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(&actor)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let child_uri = format!("{base}/objects/child");
    let leaf_uri = format!("{base}/objects/leaf");
    let child_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, uri, url, in_reply_to_uri, created_at) \
         VALUES ($1,'orphan reply','<p>orphan reply</p>','public',false,$2,$2,$3,now()) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&child_uri)
    .bind(&leaf_uri)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Seed the parent `leaf` row itself (same account — no duplicate
    // actor insert), still with NULL `in_reply_to_id`.
    let leaf_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, uri, url, created_at) \
         VALUES ($1,'remote leaf','<p>remote leaf</p>','public',false,$2,$2,now()) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&leaf_uri)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Opening the leaf's context expands it — the re-ingest links the
    // parked child reply back via `backfill_pending_replies`.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The parked reply is now linked and the parent's counter moved.
    let (reply_to, parent_count): (Option<i64>, i32) = sqlx::query_as(
        "SELECT s.in_reply_to_id, p.replies_count FROM statuses s \
         JOIN statuses p ON p.uri = $2 WHERE s.id = $1",
    )
    .bind(child_id)
    .bind(&leaf_uri)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(reply_to, Some(leaf_id), "orphan reply should relink");
    assert!(parent_count >= 1, "parent replies_count should bump");
}

// ---------------------------------------------------------------------------
// Post-grace refresh: a re-expansion materializes only the new replies.
// ---------------------------------------------------------------------------

/// Mock server whose `/objects/leaf` gains reply `r4` once `added`
/// flips — proves the post-grace refresh is incremental.
async fn serve_docs_growing_reply(
    added: Arc<std::sync::atomic::AtomicBool>,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = Router::new();
    let tick = |counter: &Arc<std::sync::atomic::AtomicUsize>| {
        let c = counter.clone();
        move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    };
    for (path, doc) in remote_docs(&base) {
        if path == "/objects/leaf" {
            continue;
        }
        let count = tick(&counter);
        app = app.route(
            path,
            get(move || {
                let doc = doc.clone();
                let count = count.clone();
                async move {
                    count();
                    Json(doc)
                }
            }),
        );
    }
    let r4 = note(&base, "r4", "actor2", Some("leaf"), Value::Null);
    let count = tick(&counter);
    app = app.route(
        "/objects/r4",
        get(move || {
            let doc = r4.clone();
            let count = count.clone();
            async move {
                count();
                Json(doc)
            }
        }),
    );
    let count = tick(&counter);
    let leaf_base = base.clone();
    let leaf_added = added.clone();
    app = app.route(
        "/objects/leaf",
        get(move || {
            let doc = note(&leaf_base, "leaf", "actor", Some("parent"), Value::Null);
            let added = leaf_added.clone();
            let count = count.clone();
            let base = leaf_base.clone();
            async move {
                count();
                let mut items = vec![
                    json!(format!("{base}/objects/r1")),
                    json!({
                        "id": format!("{base}/objects/r2"),
                        "type": "Note",
                        "attributedTo": format!("{base}/actor2"),
                        "content": "<p>r2 content</p>",
                        "published": "2026-10-30T11:00:00Z",
                        "inReplyTo": format!("{base}/objects/leaf"),
                        "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    }),
                ];
                if added.load(std::sync::atomic::Ordering::SeqCst) {
                    items.push(json!(format!("{base}/objects/r4")));
                }
                let mut doc = doc;
                doc["replies"] = json!({
                    "id": format!("{base}/replies/leaf"),
                    "type": "Collection",
                    "first": {"type": "CollectionPage", "items": items},
                });
                Json(doc)
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, counter)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_refresh_after_grace_is_incremental() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let added = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (base, counter) = serve_docs_growing_reply(added.clone()).await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    let token = register(&env.app, "refresher", "refresher@test.local").await;

    let (status, _, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_count = counter.load(std::sync::atomic::Ordering::SeqCst);
    assert!(first_count > 0);

    // Expire the grace marker, then the remote "posts" a new reply.
    sqlx::query("UPDATE status_expansions SET subtree_done_at = now() - interval '1 hour'")
        .execute(&env.state.db)
        .await
        .unwrap();
    added.store(true, std::sync::atomic::Ordering::SeqCst);

    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The refresh is incremental: the leaf doc plus the new reply —
    // ancestors and already-local replies are not re-fetched.
    let r4_uri = format!("{base}/objects/r4");
    assert!(
        status_id_by_uri(&env.state.db, &r4_uri).await.is_some(),
        "post-grace refresh should materialize the new reply: {body}"
    );
    let delta = counter.load(std::sync::atomic::Ordering::SeqCst) - first_count;
    assert_eq!(
        delta, 2,
        "refresh should cost exactly the target doc + the new reply"
    );
}

// ---------------------------------------------------------------------------
// Remote engagement counters (Section 5)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_expansion_records_remote_counts() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let (base, counter) = serve_docs_counted(|base| {
        remote_docs(base)
            .into_iter()
            .map(|(p, mut doc)| {
                if p == "/objects/leaf" {
                    doc["likes"] = json!({"id": format!("{base}/likes/leaf"), "totalItems": 7});
                    doc["shares"] = json!({"id": format!("{base}/shares/leaf"), "totalItems": 3});
                }
                (p, doc)
            })
            .collect()
    })
    .await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    let token = register(&env.app, "counter", "counter@test.local").await;

    let (status, _, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Inline `totalItems` needs no collection GET.
    let (likes, shares, fetched_at): (
        Option<i32>,
        Option<i32>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT remote_likes_count, remote_shares_count, remote_counts_fetched_at \
             FROM statuses WHERE id = $1",
    )
    .bind(leaf_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(likes, Some(7));
    assert_eq!(shares, Some(3));
    assert!(fetched_at.is_some());

    // The entity renders GREATEST(local, remote).
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["favourites_count"].as_i64(), Some(7), "{body}");
    assert_eq!(body["reblogs_count"].as_i64(), Some(3), "{body}");

    // A read inside `counts_grace` doesn't refetch the collections.
    let n = counter.load(std::sync::atomic::Ordering::SeqCst);
    let _ = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/context"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), n);
}

// ---------------------------------------------------------------------------
// Liker proxy (Section 7): origin `likes` collection pages fill
// favourited_by past the local rows, with mid-page cursor resume.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn favourited_by_proxies_remote_likes_pages() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let (base, counter) = serve_docs_counted(|base| {
        let mut docs = remote_docs(base);
        for (p, doc) in docs.iter_mut() {
            if *p == "/objects/leaf" {
                doc["likes"] = json!({
                    "id": format!("{base}/likes/leaf"),
                    "type": "OrderedCollection",
                    "totalItems": 3,
                    "first": format!("{base}/likes/leaf?page=1"),
                });
            }
        }
        docs.push((
            "/likes/leaf",
            json!({
                "id": format!("{base}/likes/leaf?page=1"),
                "type": "OrderedCollectionPage",
                "partOf": format!("{base}/likes/leaf"),
                "orderedItems": [
                    format!("{base}/actor"),
                    format!("{base}/actor2"),
                ],
                "next": format!("{base}/likes/leaf2"),
            }),
        ));
        docs.push((
            "/likes/leaf2",
            json!({
                "id": format!("{base}/likes/leaf2"),
                "type": "OrderedCollectionPage",
                "partOf": format!("{base}/likes/leaf"),
                "orderedItems": [format!("{base}/actor3")],
            }),
        ));
        docs
    })
    .await;
    let leaf_id = seed_remote_leaf(&env.state.db, &base).await;
    sqlx::query("UPDATE statuses SET remote_likes_count = 3 WHERE id = $1")
        .bind(leaf_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let token = register(&env.app, "liker", "liker@test.local").await;

    // First page: two proxied likers fill the (empty) local page.
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/favourited_by?limit=2"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page1 = body.as_array().unwrap();
    assert_eq!(page1.len(), 2, "{body}");

    // Continuation via the negative cursor: page 2 serves the
    // remainder without losing mid-page items or refetching the
    // status doc (the collection resolution is cached).
    let (status, _, body) = request(
        &env.app,
        "GET",
        &format!("/api/v1/statuses/{leaf_id}/favourited_by?limit=2&max_id=-2"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page2 = body.as_array().unwrap();
    assert_eq!(page2.len(), 1, "{body}");
    let usernames: std::collections::HashSet<String> = page1
        .iter()
        .chain(page2.iter())
        .filter_map(|a| a["acct"].as_str().map(str::to_string))
        .collect();
    assert_eq!(usernames.len(), 3, "{usernames:?}");
    // Status doc + page 1 + page 2 — the second request only fetched
    // the second page.
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------------
// Pruning (Section 8): the sweep deletes eligible context-only rows,
// keeps local anchors and whole ancestor chains of kept rows.
// ---------------------------------------------------------------------------

fn job_ctx(env: &Env) -> yonder::jobs::JobContext {
    yonder::jobs::JobContext {
        db: env.state.db.clone(),
        redis: env.state.redis.clone(),
        config: env.state.config().clone(),
        media: env.state.media.clone(),
    }
}

/// A remote status row aged past the retention window (`fetched_at`
/// 40 days back — retention is 30).
async fn seed_old_remote(db: &PgPool, account_id: i64, uri: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, uri, url, created_at, fetched_at) \
         VALUES ($1,'ctx','<p>ctx</p>','public',false,$2,$2,now(),\
                 now() - interval '40 days') \
         RETURNING id",
    )
    .bind(account_id)
    .bind(uri)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn row_exists(db: &PgPool, id: i64) -> bool {
    sqlx::query_scalar::<_, Option<i64>>("SELECT id FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await
        .unwrap()
        .is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_prunes_context_only_rows() {
    use yonder::jobs::JobHandler;

    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let _token = register(&env.app, "pruner", "pruner@test.local").await;
    let local_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'pruner' AND domain IS NULL")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    // Remote author account (not followed yet).
    let base = "http://remote.example";
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'')",
    )
    .bind("remote.example")
    .bind("author")
    .bind(format!("{base}/author"))
    .execute(&env.state.db)
    .await
    .unwrap();
    let remote_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(format!("{base}/author"))
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let db = &env.state.db;
    let uri = |n: &str| format!("{base}/objects/{n}");

    // Eligible rows — pruned.
    let gone = seed_old_remote(db, remote_id, &uri("gone")).await;
    let gone_remote_mention = seed_old_remote(db, remote_id, &uri("gone-mention")).await;
    sqlx::query("INSERT INTO status_mentions (status_id, account_id) VALUES ($1,$2)")
        .bind(gone_remote_mention)
        .bind(remote_id)
        .execute(db)
        .await
        .unwrap();

    // Kept rows — local anchors.
    let keep_mention = seed_old_remote(db, remote_id, &uri("keep-mention")).await;
    sqlx::query("INSERT INTO status_mentions (status_id, account_id) VALUES ($1,$2)")
        .bind(keep_mention)
        .bind(local_id)
        .execute(db)
        .await
        .unwrap();
    let keep_fav = seed_old_remote(db, remote_id, &uri("keep-fav")).await;
    sqlx::query("INSERT INTO favourites (account_id, status_id) VALUES ($1,$2)")
        .bind(local_id)
        .bind(keep_fav)
        .execute(db)
        .await
        .unwrap();
    let keep_selected = seed_old_remote(db, remote_id, &uri("keep-selected")).await;
    sqlx::query("UPDATE statuses SET last_selected_at = now() WHERE id = $1")
        .bind(keep_selected)
        .execute(db)
        .await
        .unwrap();
    // A whole ancestor chain survives when the leaf is kept: g ← p ← r
    // (r has a local favourite; under a single-pass block, g would be
    // deleted — the fixpoint keep-set must retain it).
    let g = seed_old_remote(db, remote_id, &uri("g")).await;
    let p = seed_old_remote(db, remote_id, &uri("p")).await;
    let r = seed_old_remote(db, remote_id, &uri("r")).await;
    sqlx::query("UPDATE statuses SET in_reply_to_id = $2 WHERE id = $1")
        .bind(p)
        .bind(g)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET in_reply_to_id = $2 WHERE id = $1")
        .bind(r)
        .bind(p)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO favourites (account_id, status_id) VALUES ($1,$2)")
        .bind(local_id)
        .bind(r)
        .execute(db)
        .await
        .unwrap();
    // A *fresh* parked reply keeps its (old, eligible) parent.
    let keep_parked = seed_old_remote(db, remote_id, &uri("parked-parent")).await;
    sqlx::query(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, uri, url, in_reply_to_uri, created_at) \
         VALUES ($1,'fresh reply','<p>fresh</p>','public',false,$2,$2,$3,now())",
    )
    .bind(remote_id)
    .bind(uri("parked-reply"))
    .bind(uri("parked-parent"))
    .execute(db)
    .await
    .unwrap();
    // A followed author's row is never context-only — authored by a
    // *second* remote account so the author-follow exclusion doesn't
    // taint the rest of the fixtures.
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'')",
    )
    .bind("followed.example")
    .bind("followed")
    .bind("http://followed.example/followed")
    .execute(db)
    .await
    .unwrap();
    let followed_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind("http://followed.example/followed")
        .fetch_one(db)
        .await
        .unwrap();
    let keep_followed = seed_old_remote(db, followed_id, &uri("keep-followed")).await;
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1,$2)")
        .bind(local_id)
        .bind(followed_id)
        .execute(db)
        .await
        .unwrap();
    // A local parent's reply: deleted, counter decremented.
    let local_parent: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, visibility, \
             local, created_at, replies_count) \
         VALUES ($1,'local','<p>local</p>','public',true,now(),1) RETURNING id",
    )
    .bind(local_id)
    .fetch_one(db)
    .await
    .unwrap();
    let dead_reply = seed_old_remote(db, remote_id, &uri("dead-reply")).await;
    sqlx::query("UPDATE statuses SET in_reply_to_id = $2 WHERE id = $1")
        .bind(dead_reply)
        .bind(local_parent)
        .execute(db)
        .await
        .unwrap();
    // A tombstone is left alone.
    let tomb = seed_old_remote(db, remote_id, &uri("tomb")).await;
    sqlx::query("UPDATE statuses SET tombstone = true WHERE id = $1")
        .bind(tomb)
        .execute(db)
        .await
        .unwrap();

    yonder::posts::cleanup::CleanupRemoteActivitiesJob
        .run(&job_ctx(&env), &json!({}))
        .await
        .unwrap();

    assert!(!row_exists(db, gone).await, "eligible row should prune");
    assert!(
        !row_exists(db, gone_remote_mention).await,
        "a remote-only mention must not block pruning"
    );
    assert!(
        !row_exists(db, dead_reply).await,
        "context reply should prune"
    );
    assert!(row_exists(db, keep_mention).await, "local mention keeps");
    assert!(row_exists(db, keep_fav).await, "local favourite keeps");
    assert!(
        row_exists(db, keep_selected).await,
        "recent selection keeps"
    );
    assert!(row_exists(db, keep_followed).await, "followed author keeps");
    assert!(
        row_exists(db, keep_parked).await,
        "parked reference keeps parent"
    );
    assert!(row_exists(db, r).await);
    assert!(row_exists(db, p).await, "parent of kept reply survives");
    assert!(
        row_exists(db, g).await,
        "grandparent of kept reply survives via the fixpoint"
    );
    assert!(row_exists(db, tomb).await, "tombstones are not pruned");
    assert!(row_exists(db, local_parent).await);
    let count: i32 = sqlx::query_scalar("SELECT replies_count FROM statuses WHERE id = $1")
        .bind(local_parent)
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(count, 0, "deleted reply must decrement replies_count");
}

// ---------------------------------------------------------------------------
// ThreadPath (Section 6): a followed author's orphan reply gets an
// ancestor-path job; a stranger's doesn't.
// ---------------------------------------------------------------------------

/// Decode every job envelope on the `default` stream.
async fn default_queue(env: &Env) -> Vec<yonder::jobs::JobEnvelope> {
    use redis::AsyncCommands;
    let key = env.state.redis.key("queue:default");
    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn.xrange(&key, "-", "+").await.unwrap();
    let mut out = Vec::new();
    for entry in reply.ids {
        if let Some(v) = entry.map.get("job") {
            let body: String = redis::from_redis_value(v).unwrap();
            out.push(yonder::jobs::queue::decode(&body).unwrap());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_path_job_relinks_followed_author_reply() {
    use yonder::jobs::{JobHandler, JobKind};

    let _permit = harness::acquire().await;
    let Some(env) = test_env().await else {
        return;
    };
    let base = serve_docs(remote_docs).await;
    let _token = register(&env.app, "follower", "follower@test.local").await;
    let local_id: i64 = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE username = 'follower' AND domain IS NULL",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // Two remote authors — one followed, one a stranger.
    for (name, actor) in [("actor", "actor"), ("actor2", "actor2")] {
        sqlx::query(
            "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
             VALUES ($1,$2,$3,$3,'')",
        )
        .bind(base.trim_start_matches("http://"))
        .bind(name)
        .bind(format!("{base}/{actor}"))
        .execute(&env.state.db)
        .await
        .unwrap();
    }
    let followed_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(format!("{base}/actor"))
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1,$2)")
        .bind(local_id)
        .bind(followed_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let ap = env.state.ap.clone().unwrap();
    let deliver = |actor: &str, obj: Value| {
        let raw = json!({
            "id": format!("{}/objects/{}#activity", base, obj["id"].as_str().unwrap().rsplit('/').next().unwrap()),
            "type": "Create",
            "actor": format!("{base}/{actor}"),
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": obj,
        });
        let activity = pubby_sdk::Activity::from_json(&raw);
        (activity, raw)
    };
    let note_obj = |name: &str, author: &str, reply_to: &str| {
        json!({
            "id": format!("{base}/objects/{name}"),
            "type": "Note",
            "attributedTo": format!("{base}/{author}"),
            "content": format!("<p>{name}</p>"),
            "published": "2026-10-30T12:00:00Z",
            "inReplyTo": reply_to,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        })
    };

    // Followed author's reply to an unstored parent → ThreadPath.
    let reply_uri = format!("{base}/objects/freply");
    let (activity, raw) = deliver(
        "actor",
        note_obj("freply", "actor", &format!("{base}/objects/parent")),
    );
    assert!(yonder::posts::remote::handle(&ap.core, &activity, &raw).await);
    let reply_id = status_id_by_uri(&env.state.db, &reply_uri).await.unwrap();
    let thread_jobs = default_queue(&env)
        .await
        .into_iter()
        .filter(|j| j.kind == JobKind::ThreadPath)
        .count();
    assert_eq!(
        thread_jobs, 1,
        "followed author's orphan reply should enqueue ThreadPath"
    );

    // Stranger's reply to an unstored parent → nothing.
    let stranger_uri = format!("{base}/objects/sreply");
    let (activity, raw) = deliver(
        "actor2",
        note_obj("sreply", "actor2", &format!("{base}/objects/nowhere")),
    );
    assert!(yonder::posts::remote::handle(&ap.core, &activity, &raw).await);
    assert!(status_id_by_uri(&env.state.db, &stranger_uri)
        .await
        .is_some());
    let thread_jobs = default_queue(&env)
        .await
        .into_iter()
        .filter(|j| j.kind == JobKind::ThreadPath)
        .count();
    assert_eq!(thread_jobs, 1, "stranger reply must not enqueue ThreadPath");

    // The job materializes the parent and relinks.
    yonder::activitypub::thread::ThreadPathJob::new(ap)
        .run(&job_ctx(&env), &json!({"status_id": reply_id}))
        .await
        .unwrap();
    let (reply_to, parent_count): (Option<i64>, i32) = sqlx::query_as(
        "SELECT s.in_reply_to_id, p.replies_count FROM statuses s \
         JOIN statuses p ON p.id = s.in_reply_to_id WHERE s.id = $1",
    )
    .bind(reply_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(reply_to.is_some(), "path job must relink the reply");
    assert_eq!(parent_count, 1, "relink bumps replies_count");
}
