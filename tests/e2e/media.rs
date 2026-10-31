//! Epic-18 media tests: the `MediaStore` abstraction on the local
//! backend, remote-media fetch + cache, and the `CleanupRemoteMedia`
//! sweep (age/size thresholds + the boost/quote protection rule).
//! Skipped when no container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{body::Body, http::Request, Router};
use serde_json::json;
use testcontainers::{runners::AsyncRunner, ImageExt};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{Dispatcher, JobContext, JobHandler},
    media::jobs::*,
};

use crate::harness;

struct Env {
    app: Router,
    state: Arc<AppState>,
}

async fn test_app(config: Config) -> Option<Env> {
    test_app_with(config, false).await
}

/// Same as [`test_app`] but with a built `ApContext` — required for
/// the code paths gated on `state.ap` (remote-actor refresh enqueues,
/// the ingest media hook).
async fn test_app_federated(mut config: Config) -> Option<Env> {
    config.federation.enabled = true;
    test_app_with(config, true).await
}

async fn test_app_with(mut config: Config, federated: bool) -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-media-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();
    config.federation.base_url = Some("http://127.0.0.1:4000".to_string());
    // The account reads sit in the `timeline` bucket — raise it so the
    // test's repeated requests don't 429. `account_creation` covers
    // the register() helper's tokens.
    config.rate_limit.timeline = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.timeline.clone();
    // Loopback fixtures need the relaxed fetcher.
    config.federation.allow_private_targets = true;
    let mut state = AppState::new(config, pool, redis_handle);
    if federated {
        let ap = activitypub::build(
            state.config().as_ref(),
            &state.db,
            &state.redis,
            &Dispatcher::new(&state.redis, &state.config().jobs),
            &state.bus,
        )
        .await
        .unwrap();
        state.ap = Some(ap);
    }
    let state = Arc::new(state);
    let app = build_router(state.clone());
    Some(Env { app, state })
}

fn job_ctx(state: &AppState) -> JobContext {
    JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    }
}

async fn remote_account(db: &sqlx::PgPool, actor_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id) \
         VALUES ('remote.test', 'remote', $1) RETURNING id",
    )
    .bind(actor_id)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn remote_status(db: &sqlx::PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, visibility, local) \
         VALUES ($1, 'remote post', 'public', false) RETURNING id",
    )
    .bind(account_id)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A status row authored by a local account (`local = true`).
async fn local_status(db: &sqlx::PgPool, account_id: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, visibility, local) \
         VALUES ($1, 'post', 'public', true) RETURNING id",
    )
    .bind(account_id)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn media_row(
    db: &sqlx::PgPool,
    account_id: i64,
    status_id: Option<i64>,
    remote_url: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO media_attachments (account_id, status_id, type, remote_url, content_type) \
         VALUES ($1,$2,'image',$3,'image/png') RETURNING id",
    )
    .bind(account_id)
    .bind(status_id)
    .bind(remote_url)
    .fetch_one(db)
    .await
    .unwrap()
}

fn png() -> Vec<u8> {
    // 1x1 PNG.
    [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
    .to_vec()
}

/// A loopback HTTP origin serving `png()` at `/x.png`, counting hits.
async fn counted_origin() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let app = axum::Router::new().route(
        "/x.png",
        axum::routing::get(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                png()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/x.png"), hits)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_store_roundtrip_and_serve() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(Config::default()).await else {
        return;
    };
    let store = &env.state.media;
    store
        .put("avatars/1/x.png", &png(), "image/png")
        .await
        .unwrap();
    assert_eq!(store.get("avatars/1/x.png").await.unwrap(), png());
    let listed = store.list_prefix("avatars/1/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "avatars/1/x.png");

    // The serving route proxies the store on every backend.
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/avatars/1/x.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
    assert!(res
        .headers()
        .get("cache-control")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("immutable"));

    // delete_account_files removes the account's tree.
    yonder::media::storage::delete_account_files(store, 1)
        .await
        .unwrap();
    assert!(matches!(
        store.get("avatars/1/x.png").await,
        Err(yonder::Error::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_media_fetch_caches() {
    use std::sync::atomic::Ordering;
    let mut config = Config::default();
    config.media.fetch_remote_media = true;
    let _permit = harness::acquire().await;
    let Some(env) = test_app(config).await else {
        return;
    };
    let (url, hits) = counted_origin().await;
    let acct = remote_account(&env.state.db, "http://remote.test/u/r").await;
    let status = remote_status(&env.state.db, acct).await;
    let mid = media_row(&env.state.db, acct, Some(status), &url).await;

    RemoteMediaFetchJob
        .run(&job_ctx(&env.state), &json!({"media_id": mid}))
        .await
        .unwrap();
    let key: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM media_attachments WHERE id = $1")
            .bind(mid)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    let key = key.expect("cached file_path");
    let stored = env.state.media.get(&key).await.unwrap();
    let file_size: i64 =
        sqlx::query_scalar("SELECT file_size FROM media_attachments WHERE id = $1")
            .bind(mid)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(stored.len() as i64, file_size);
    // remote_url survives as provenance.
    let remote: String =
        sqlx::query_scalar("SELECT remote_url FROM media_attachments WHERE id = $1")
            .bind(mid)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(remote, url);

    // Re-running the job on a cached row is a no-op — the origin must
    // not be hit a second time and `file_path` stays put.
    RemoteMediaFetchJob
        .run(&job_ctx(&env.state), &json!({"media_id": mid}))
        .await
        .unwrap();
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "cached rows must not re-download"
    );
    let key2: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM media_attachments WHERE id = $1")
            .bind(mid)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(key2.as_deref(), Some(key.as_str()));
}

/// Seed a cached remote attachment whose post is (or isn't) boosted /
/// quoted by a local account, then run the cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_media_cleanup_protects_boosts_and_quotes() {
    let mut config = Config::default();
    config.media.remote_cleanup_max_age_days = 0; // everything is "old"
    config.media.remote_cleanup_max_bytes = 0;
    let _permit = harness::acquire().await;
    let Some(env) = test_app(config).await else {
        return;
    };
    let db = &env.state.db;
    let remote = remote_account(db, "http://remote.test/u/r").await;
    let boosted_status = remote_status(db, remote).await;
    let quoted_status = remote_status(db, remote).await;
    let plain_status = remote_status(db, remote).await;
    // A local account boosts one remote post and quotes another.
    let local = sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (domain, username, actor_id) \
         VALUES (NULL, 'localuser', 'http://127.0.0.1:4000/ap/users/localuser') RETURNING id",
    )
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO statuses (account_id, reblog_of_id, local, visibility) \
         VALUES ($1,$2,true,'public')",
    )
    .bind(local)
    .bind(boosted_status)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO statuses (account_id, quote_of_id, local, visibility) \
         VALUES ($1,$2,true,'public')",
    )
    .bind(local)
    .bind(quoted_status)
    .execute(db)
    .await
    .unwrap();

    // Three cached rows: protected-by-boost, protected-by-quote,
    // unprotected.
    let store = &env.state.media;
    let mut ids = Vec::new();
    for status in [boosted_status, quoted_status, plain_status] {
        let mid = media_row(db, remote, Some(status), "http://remote.test/m.png").await;
        let key = format!("media_attachments/{remote}/{mid}.png");
        store.put(&key, &png(), "image/png").await.unwrap();
        sqlx::query("UPDATE media_attachments SET file_path = $2, file_size = 4 WHERE id = $1")
            .bind(mid)
            .bind(&key)
            .execute(db)
            .await
            .unwrap();
        ids.push((mid, key));
    }
    // A local favourite must NOT protect.
    sqlx::query(
        "INSERT INTO favourites (account_id, status_id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(local)
    .bind(plain_status)
    .execute(db)
    .await
    .unwrap();

    CleanupRemoteMediaJob
        .run(&job_ctx(&env.state), &json!({}))
        .await
        .unwrap();

    let paths: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT file_path FROM media_attachments WHERE id = ANY($1) ORDER BY id",
    )
    .bind(ids.iter().map(|(id, _)| *id).collect::<Vec<_>>())
    .fetch_all(db)
    .await
    .unwrap();
    assert!(paths[0].is_some(), "boosted post's media must survive");
    assert!(paths[1].is_some(), "quoted post's media must survive");
    assert!(
        paths[2].is_none(),
        "favourited-but-not-boosted media evicted"
    );
    // Row + remote_url survive eviction; only the object is gone.
    let remote_url: Option<String> =
        sqlx::query_scalar("SELECT remote_url FROM media_attachments WHERE id = $1")
            .bind(ids[2].0)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(remote_url.as_deref(), Some("http://remote.test/m.png"));
    assert!(env.state.media.get(&ids[2].1).await.is_err());

    // Idempotent: a second run clears nothing else and leaves the
    // protected rows untouched.
    CleanupRemoteMediaJob
        .run(&job_ctx(&env.state), &json!({}))
        .await
        .unwrap();
    let paths2: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT file_path FROM media_attachments WHERE id = ANY($1) ORDER BY id",
    )
    .bind(ids.iter().map(|(id, _)| *id).collect::<Vec<_>>())
    .fetch_all(db)
    .await
    .unwrap();
    assert_eq!(paths, paths2, "second cleanup run must be a no-op");
}

/// Size and age thresholds are independent paths: a row that is not
/// old still evicts when the byte ceiling is exceeded, and a row under
/// the byte ceiling still evicts when it is too old.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_media_cleanup_age_and_size_paths() {
    // One environment for all three cases — the thresholds live in
    // `JobContext.config`, so each case overrides a clone rather than
    // paying for fresh containers. The "neither" case also pins the
    // `u64 → i64` ceiling clamp: an out-of-range
    // `remote_cleanup_max_bytes` must saturate, never wrap negative.
    let _permit = harness::acquire().await;
    let Some(env) = test_app(Config::default()).await else {
        return;
    };
    for (age_days, max_bytes, case, evicted) in [
        (3650, 0u64, "size", true),
        (0, i64::MAX as u64, "age", true),
        (3650, u64::MAX, "neither", false),
    ] {
        let mut config = (*env.state.config()).clone();
        config.media.remote_cleanup_max_age_days = age_days;
        config.media.remote_cleanup_max_bytes = max_bytes;
        let ctx = JobContext {
            db: env.state.db.clone(),
            redis: env.state.redis.clone(),
            config: Arc::new(config),
            media: env.state.media.clone(),
        };
        let db = &env.state.db;
        // Reset between cases so each candidate set is exactly this
        // case's row (the actor_id unique index needs a fresh actor too).
        sqlx::query("DELETE FROM media_attachments")
            .execute(db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM statuses")
            .execute(db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM accounts")
            .execute(db)
            .await
            .unwrap();
        let remote = remote_account(db, "http://remote.test/u/r").await;
        let status = remote_status(db, remote).await;
        let mid = media_row(db, remote, Some(status), "http://remote.test/m.png").await;
        let key = format!("media_attachments/{remote}/{mid}.png");
        env.state
            .media
            .put(&key, &png(), "image/png")
            .await
            .unwrap();
        sqlx::query("UPDATE media_attachments SET file_path = $2, file_size = 8 WHERE id = $1")
            .bind(mid)
            .bind(&key)
            .execute(db)
            .await
            .unwrap();

        CleanupRemoteMediaJob.run(&ctx, &json!({})).await.unwrap();
        let path: Option<String> =
            sqlx::query_scalar("SELECT file_path FROM media_attachments WHERE id = $1")
                .bind(mid)
                .fetch_one(db)
                .await
                .unwrap();
        assert_eq!(
            path.is_none(),
            evicted,
            "{case} case: file_path eviction mismatch"
        );
    }
}

/// A local account row — `accounts_actor_id_key` requires a unique
/// actor URL per fixture; no key material is needed for reads.
async fn local_account(db: &sqlx::PgPool, username: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id) \
         VALUES (NULL, $1, $2) RETURNING id",
    )
    .bind(username)
    .bind(format!("http://127.0.0.1:4000/ap/users/{username}"))
    .fetch_one(db)
    .await
    .unwrap()
}

async fn get_json(app: &Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_entity_and_statuses_tab() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(Config::default()).await else {
        return;
    };
    let db = &env.state.db;
    let alice = local_account(db, "alice").await;
    let bob = local_account(db, "bob").await;
    // One plain post, one reply to bob's post, one post with media.
    let bob_post = local_status(db, bob).await;
    let post = local_status(db, alice).await;
    let _reply: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, visibility, local, in_reply_to_id, in_reply_to_account_id) \
         VALUES ($1,'r','public',true,$2,$3) RETURNING id",
    )
    .bind(alice)
    .bind(bob_post)
    .bind(bob)
    .fetch_one(db)
    .await
    .unwrap();
    let with_media = local_status(db, alice).await;
    let _mid = media_row(db, alice, Some(with_media), "http://remote.test/m.png").await;

    // Account entity: full header contract.
    let (code, entity) = get_json(&env.app, &format!("/api/v1/accounts/{alice}")).await;
    assert_eq!(code, 200);
    assert_eq!(entity["acct"], "alice");
    assert_eq!(entity["locked"], false);
    assert_eq!(entity["bot"], false);
    assert!(entity["created_at"].is_string());
    assert!(entity["last_status_at"].is_string());
    assert!(entity["fields"].is_array());
    assert!(entity["header"].is_string() && entity["avatar"].is_string());

    // Default tab: posts + replies, boosts included.
    let (code, all) = get_json(&env.app, &format!("/api/v1/accounts/{alice}/statuses")).await;
    assert_eq!(code, 200);
    assert_eq!(all.as_array().unwrap().len(), 3);

    // only_media → one row.
    let (_, media) = get_json(
        &env.app,
        &format!("/api/v1/accounts/{alice}/statuses?only_media=true"),
    )
    .await;
    assert_eq!(media.as_array().unwrap().len(), 1);
    assert_eq!(media[0]["media_attachments"].as_array().unwrap().len(), 1);

    // exclude_replies → reply hidden, posts + media post remain.
    let (_, noreplies) = get_json(
        &env.app,
        &format!("/api/v1/accounts/{alice}/statuses?exclude_replies=true"),
    )
    .await;
    assert_eq!(noreplies.as_array().unwrap().len(), 2);

    // exclude_reblogs → boosts hidden.
    let boost: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, reblog_of_id, local, visibility) \
         VALUES ($1,$2,true,'public') RETURNING id",
    )
    .bind(alice)
    .bind(bob_post)
    .fetch_one(db)
    .await
    .unwrap();
    let (_, with_boost) = get_json(&env.app, &format!("/api/v1/accounts/{alice}/statuses")).await;
    assert_eq!(with_boost.as_array().unwrap().len(), 4);
    let (_, noboosts) = get_json(
        &env.app,
        &format!("/api/v1/accounts/{alice}/statuses?exclude_reblogs=true"),
    )
    .await;
    assert_eq!(noboosts.as_array().unwrap().len(), 3);
    let boost_id = boost.to_string();
    assert!(noboosts
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["id"] != boost_id));

    // pinned → only the pinned status, pin order.
    let pinned: i64 = sqlx::query_scalar(
        "INSERT INTO status_pins (account_id, status_id, idx) \
         VALUES ($1,$2,0) RETURNING status_id",
    )
    .bind(alice)
    .bind(with_media)
    .fetch_one(db)
    .await
    .unwrap();
    let (_, pins) = get_json(
        &env.app,
        &format!("/api/v1/accounts/{alice}/statuses?pinned=true"),
    )
    .await;
    let pins = pins.as_array().unwrap();
    assert_eq!(pins.len(), 1);
    let pinned_id = pinned.to_string();
    assert_eq!(pins[0]["id"], pinned_id);

    // The pin set is one list: `limit` must not page it and no Link
    // cursor is emitted (a max_id here would loop on the same page).
    sqlx::query("INSERT INTO status_pins (account_id, status_id, idx) VALUES ($1,$2,1)")
        .bind(alice)
        .bind(post)
        .execute(db)
        .await
        .unwrap();
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/accounts/{alice}/statuses?pinned=true&limit=1"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers().get("link").is_none(),
        "pinned must not emit Link cursors"
    );
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let pins: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let pins = pins.as_array().unwrap();
    assert_eq!(pins.len(), 2, "all pins returned regardless of limit");
    assert_eq!(pins[0]["id"], pinned_id);

    // Pagination: limit caps the page and emits Link cursors.
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/accounts/{alice}/statuses?limit=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let link = res
        .headers()
        .get("link")
        .map(|h| h.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        link.contains("rel=\"next\""),
        "expected next cursor, got {link:?}"
    );
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let page1: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page1.as_array().unwrap().len(), 1);

    // Visibility: a followers-only status is invisible to an
    // anonymous viewer.
    sqlx::query(
        "INSERT INTO statuses (account_id, content, visibility, local) \
         VALUES ($1,'gated','private',true)",
    )
    .bind(alice)
    .execute(db)
    .await
    .unwrap();
    let (_, anon) = get_json(&env.app, &format!("/api/v1/accounts/{alice}/statuses")).await;
    let anon = anon.as_array().unwrap();
    assert_eq!(anon.len(), 4);
    assert!(anon.iter().all(|s| s["visibility"] != "private"));

    // A limited (silenced) author's profile timeline is still
    // served — the owner and followers always see it, and
    // non-followers get a client-side notice + "Show posts anyway".
    sqlx::query("UPDATE accounts SET limited_at = now() WHERE id = $1")
        .bind(alice)
        .execute(db)
        .await
        .unwrap();
    let (code, body) = get_json(&env.app, &format!("/api/v1/accounts/{alice}/statuses")).await;
    assert_eq!(code, 200);
    assert_eq!(body.as_array().unwrap().len(), 4);
    sqlx::query("UPDATE accounts SET limited_at = NULL WHERE id = $1")
        .bind(alice)
        .execute(db)
        .await
        .unwrap();

    // Missing accounts 404; suspended accounts render with the
    // `suspended` tombstone flag (the profile shows a badge + grayed
    // avatar instead of a bare 404).
    let (code, _) = get_json(&env.app, "/api/v1/accounts/999999").await;
    assert_eq!(code, 404);
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice)
        .execute(db)
        .await
        .unwrap();
    let (code, body) = get_json(&env.app, &format!("/api/v1/accounts/{alice}")).await;
    assert_eq!(code, 200);
    assert_eq!(body["suspended"], true);
}

/// `max_upload_size` is enforced in the `store_status_media` seam, so
/// it applies on every backend — an oversized upload is rejected
/// before a single byte is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_upload_rejected() {
    let mut config = Config::default();
    config.media.max_upload_size = 64;
    let _permit = harness::acquire().await;
    let Some(env) = test_app(config).await else {
        return;
    };
    let alice = local_account(&env.state.db, "alice").await;
    let err = match yonder::media::store_status_media(
        &env.state.config(),
        &env.state.media,
        &env.state.db,
        alice,
        vec![0u8; 128],
        yonder::media::attachments::UploadMeta::default(),
    )
    .await
    {
        Ok(_) => panic!("oversized upload must be rejected"),
        Err(e) => e,
    };
    assert!(matches!(err, yonder::Error::Validation(_)));
    // Nothing landed in the store.
    assert!(env
        .state
        .media
        .list_prefix(&format!("media_attachments/{alice}/"))
        .await
        .unwrap()
        .is_empty());
}

/// pg + redis + an S3 backend pointed at `s3_endpoint` — shared setup
/// for the MinIO and stub S3 tests. `None` when no container runtime.
async fn s3_env(s3_endpoint: &str) -> Option<Env> {
    let mut config = Config::default();
    config.media.backend = yonder::config::MediaBackend::S3;
    config.media.s3_endpoint = Some(s3_endpoint.to_string());
    config.media.s3_bucket = Some("media".to_string());
    config.media.s3_access_key = Some("minioadmin".to_string());
    config.media.s3_secret_key = Some("minioadmin".to_string());
    config.media.s3_region = Some("us-east-1".to_string());
    test_app(config).await
}

/// The assertion sequence every S3 store must satisfy, independent of
/// which origin backs it: put/get/list through `MediaStore`, proxied
/// serving through `GET /media/{key}` with the same headers as the
/// local backend, and `delete_account_files` draining the account's
/// prefixes.
async fn assert_s3_roundtrip(env: &Env) {
    let store = &env.state.media;
    store
        .put("avatars/1/x.png", &png(), "image/png")
        .await
        .unwrap();
    assert_eq!(store.get("avatars/1/x.png").await.unwrap(), png());
    let listed = store.list_prefix("avatars/1/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].size, png().len() as u64);

    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/media/avatars/1/x.png")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers().get("content-type").unwrap(), "image/png");
    assert!(res
        .headers()
        .get("cache-control")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("immutable"));

    yonder::media::storage::delete_account_files(store, 1)
        .await
        .unwrap();
    assert!(matches!(
        store.get("avatars/1/x.png").await,
        Err(yonder::Error::NotFound(_))
    ));
}

/// The S3 backend end-to-end against a real MinIO container. Skipped
/// when the `minio/minio` image can't be pulled (registry restrictions
/// in CI) — `s3_store_roundtrip_against_stub` covers the same
/// assertions everywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_store_roundtrip_minio() {
    use testcontainers_modules::minio::MinIO;
    if !harness::docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return;
    }
    let Ok(minio) = MinIO::default()
        .with_env_var("MINIO_ROOT_USER", "minioadmin")
        .with_env_var("MINIO_ROOT_PASSWORD", "minioadmin")
        .start()
        .await
    else {
        eprintln!("skipping: minio image not pullable in this environment");
        return;
    };
    let s3_endpoint = format!(
        "http://127.0.0.1:{}",
        minio.get_host_port_ipv4(9000).await.unwrap()
    );

    // The store doesn't create buckets — provision it first (MinIO's
    // "API:" ready log can precede the listener by a beat, so retry).
    let bucket_cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1".to_string()))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            "minioadmin",
            "minioadmin",
            None,
            None,
            "test",
        ))
        .endpoint_url(&s3_endpoint)
        .force_path_style(true)
        .build();
    let bucket_client = aws_sdk_s3::Client::from_conf(bucket_cfg);
    for attempt in 0..10 {
        match bucket_client.create_bucket().bucket("media").send().await {
            Ok(_) => break,
            Err(_) if attempt == 9 => panic!("minio never accepted create_bucket"),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }

    let _permit = harness::acquire().await;
    let Some(env) = s3_env(&s3_endpoint).await else {
        return;
    };
    assert_s3_roundtrip(&env).await;
}

/// A minimal in-memory S3 stub — path-style addressing, covering the
/// four operations `S3Store` uses (put/get/delete/list-prefix). It
/// exists so the S3 code path is exercised where MinIO can't be
/// pulled; it is not a substitute for a real-bucket conformance run.
mod fake_s3 {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::{
        body::Bytes,
        extract::{Path as AxumPath, Query, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::put,
        Router,
    };

    pub type Objects = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Serve the stub on loopback; returns its base URL.
    pub async fn serve() -> String {
        let objects: Objects = Arc::new(Mutex::new(HashMap::new()));
        let app = Router::new()
            .route(
                "/media/{*key}",
                put(put_object).get(get_object).delete(delete_object),
            )
            .route("/media", axum::routing::get(list_objects))
            // Path-style ListObjectsV2 hits `{endpoint}/{bucket}/`.
            .route("/media/", axum::routing::get(list_objects))
            .with_state(objects);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn put_object(
        State(objects): State<Objects>,
        AxumPath(key): AxumPath<String>,
        body: Bytes,
    ) -> StatusCode {
        objects.lock().unwrap().insert(key, body.to_vec());
        StatusCode::OK
    }

    async fn get_object(
        State(objects): State<Objects>,
        AxumPath(key): AxumPath<String>,
    ) -> Response {
        match objects.lock().unwrap().get(&key) {
            Some(bytes) => (StatusCode::OK, bytes.clone()).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                "<Error><Code>NoSuchKey</Code></Error>",
            )
                .into_response(),
        }
    }

    async fn delete_object(
        State(objects): State<Objects>,
        AxumPath(key): AxumPath<String>,
    ) -> StatusCode {
        objects.lock().unwrap().remove(&key);
        StatusCode::NO_CONTENT
    }

    async fn list_objects(
        State(objects): State<Objects>,
        Query(params): Query<HashMap<String, String>>,
    ) -> String {
        let prefix = params.get("prefix").cloned().unwrap_or_default();
        let contents = objects
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| {
                format!(
                    "<Contents><Key>{k}</Key><Size>{}</Size></Contents>",
                    v.len()
                )
            })
            .collect::<String>();
        format!(
            "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>media</Name><Prefix>{prefix}</Prefix><IsTruncated>false</IsTruncated>\
             {contents}</ListBucketResult>"
        )
    }
}

/// The S3 backend against the in-process stub — exercises SDK request
/// building (endpoint, path-style, SigV4 headers the stub ignores) and
/// response parsing everywhere, no container image needed beyond
/// pg/redis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_store_roundtrip_against_stub() {
    let s3_endpoint = fake_s3::serve().await;
    let _permit = harness::acquire().await;
    let Some(env) = s3_env(&s3_endpoint).await else {
        return;
    };
    assert_s3_roundtrip(&env).await;
}

// ---------------------------------------------------------------------------
// Federation-gated paths (ApContext required) + remaining review tests
// ---------------------------------------------------------------------------

/// Register a local account through the API; returns the user token.
async fn register(app: &Router, username: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::post("/api/v1/accounts")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "username": username,
                        "email": format!("{username}@example.com"),
                        "password": "hunter2-password",
                        "agreement": true,
                        "locale": "en",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    body["access_token"].as_str().unwrap().to_string()
}

async fn get_json_authed(
    app: &Router,
    uri: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

/// Parsed `job` envelopes on the default queue — the stream both
/// `RemoteActorFetch` and `RemoteMediaFetch` land on.
async fn default_queue_jobs(env: &Env) -> Vec<serde_json::Value> {
    let key = env.state.redis.key("queue:default");
    let mut conn = env
        .state
        .redis
        .client()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let raw: Vec<(String, Vec<String>)> = redis::cmd("XRANGE")
        .arg(&key)
        .arg("-")
        .arg("+")
        .query_async(&mut conn)
        .await
        .unwrap();
    raw.iter()
        .flat_map(|(_, fields)| {
            fields
                .chunks(2)
                .filter(|pair| pair[0] == "job")
                .map(|pair| pair[1].clone())
                .collect::<Vec<_>>()
        })
        .filter_map(|job| serde_json::from_str::<serde_json::Value>(&job).ok())
        .collect()
}

/// A stale remote profile enqueues `RemoteActorFetch` once — repeat
/// reads inside the throttle window must not pile up duplicates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_profile_refresh_enqueue_is_throttled() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app_federated(Config::default()).await else {
        return;
    };
    // No actor_cache row → stale on first read.
    let remote = remote_account(&env.state.db, "http://remote.test/u/r").await;
    for _ in 0..3 {
        let (code, entity) = get_json(&env.app, &format!("/api/v1/accounts/{remote}")).await;
        assert_eq!(code, 200, "{entity}");
    }
    let jobs = default_queue_jobs(&env).await;
    let fetches = jobs
        .iter()
        .filter(|j| j["kind"] == "remote_actor_fetch")
        .count();
    assert_eq!(fetches, 1, "expected one RemoteActorFetch, got {jobs:?}");
}

/// Positive case for the follower gate: a followers-only status is
/// served to an accepted follower and withheld from a non-follower.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn followers_only_status_visible_to_follower() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(Config::default()).await else {
        return;
    };
    let db = &env.state.db;
    let bob_token = register(&env.app, "bob").await;
    let carol_token = register(&env.app, "carol").await;
    let alice = local_account(db, "alice").await;
    let bob = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM accounts WHERE username = 'bob' AND domain IS NULL",
    )
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO statuses (account_id, content, visibility, local) \
                 VALUES ($1,'gated','followers_only',true)",
    )
    .bind(alice)
    .execute(db)
    .await
    .unwrap();
    // bob follows alice; carol doesn't.
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1,$2)")
        .bind(bob)
        .bind(alice)
        .execute(db)
        .await
        .unwrap();

    let uri = format!("/api/v1/accounts/{alice}/statuses");
    let (code, for_bob) = get_json_authed(&env.app, &uri, &bob_token).await;
    assert_eq!(code, 200, "{for_bob}");
    let for_bob = for_bob.as_array().unwrap();
    assert_eq!(for_bob.len(), 1);
    assert_eq!(for_bob[0]["visibility"], "private");

    let (code, for_carol) = get_json_authed(&env.app, &uri, &carol_token).await;
    assert_eq!(code, 200, "{for_carol}");
    assert!(for_carol.as_array().unwrap().is_empty());
}

/// The ingest hook's dedup predicate: re-ingesting a status whose
/// attachments are already cached enqueues nothing; only rows still
/// pointing at the remote get a `RemoteMediaFetch`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_media_enqueue_skips_cached_rows() {
    let mut config = Config::default();
    config.media.fetch_remote_media = true;
    let _permit = harness::acquire().await;
    let Some(env) = test_app_federated(config).await else {
        return;
    };
    let db = &env.state.db;
    let remote = remote_account(db, "http://remote.test/u/r").await;
    let status = remote_status(db, remote).await;
    // One cached row (file_path set) and one still remote-only.
    let cached = media_row(db, remote, Some(status), "http://remote.test/a.png").await;
    let key = format!("media_attachments/{remote}/{cached}.png");
    env.state
        .media
        .put(&key, &png(), "image/png")
        .await
        .unwrap();
    sqlx::query("UPDATE media_attachments SET file_path = $2 WHERE id = $1")
        .bind(cached)
        .bind(&key)
        .execute(db)
        .await
        .unwrap();
    let uncached = media_row(db, remote, Some(status), "http://remote.test/b.png").await;

    let core = env.state.ap.as_ref().unwrap().core.clone();
    yonder::posts::remote::enqueue_remote_media_fetches(&core, status)
        .await
        .unwrap();

    let jobs = default_queue_jobs(&env).await;
    let fetches: Vec<i64> = jobs
        .iter()
        .filter(|j| j["kind"] == "remote_media_fetch")
        .filter_map(|j| j["payload"]["media_id"].as_i64())
        .collect();
    assert_eq!(fetches, vec![uncached], "cached rows must not enqueue");
}

/// The write-back claims the row: a second persist on an
/// already-cached (or deleted) row loses, and the loser's just-stored
/// object is deleted rather than orphaned in the backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persist_remote_media_loser_object_is_deleted() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(Config::default()).await else {
        return;
    };
    let ctx = job_ctx(&env.state);
    let db = &env.state.db;
    let acct = remote_account(db, "http://remote.test/u/r").await;
    let status = remote_status(db, acct).await;
    let mid = media_row(db, acct, Some(status), "http://remote.test/m.png").await;
    let store = &env.state.media;
    let fetched = async |name: &str| -> FetchedMedia {
        let key = format!("media_attachments/{acct}/{name}");
        store.put(&key, &png(), "image/png").await.unwrap();
        FetchedMedia {
            key,
            kind: "image",
            content_type: "image/png".to_string(),
            size: png().len() as i64,
            blurhash: None,
            width: None,
            height: None,
            processed: false,
            exif_stripped: false,
        }
    };

    // First writer claims the row.
    let first = fetched("a.png").await;
    assert!(persist_remote_media(&ctx, mid, &first).await.unwrap());

    // A concurrent delivery that stored its own object loses the
    // write-back — its object is deleted and `file_path` keeps key1.
    let loser = fetched("b.png").await;
    assert!(!persist_remote_media(&ctx, mid, &loser).await.unwrap());
    assert!(
        env.state.media.get(&loser.key).await.is_err(),
        "loser's object must be deleted"
    );
    let file_path: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM media_attachments WHERE id = $1")
            .bind(mid)
            .fetch_one(db)
            .await
            .unwrap();
    assert_eq!(file_path.as_deref(), Some(first.key.as_str()));

    // A row deleted mid-fetch: same — the stored object is dropped.
    sqlx::query("DELETE FROM media_attachments WHERE id = $1")
        .bind(mid)
        .execute(db)
        .await
        .unwrap();
    let orphan = fetched("c.png").await;
    assert!(!persist_remote_media(&ctx, mid, &orphan).await.unwrap());
    assert!(env.state.media.get(&orphan.key).await.is_err());

    // A failed write-back: an UPDATE error does not prove the write
    // lost — a dropped connection after Postgres commits surfaces as
    // an error too — so the error path re-reads the row and drops the
    // object only when it is provably unreferenced. A table lock held
    // by another session forces the UPDATE to time out: SHARE ROW
    // EXCLUSIVE conflicts with the UPDATE's ROW EXCLUSIVE but not with
    // the re-read's ACCESS SHARE, so the lock pins the ambiguous
    // commit case that a row-level fault can't reach.
    let mid2 = media_row(db, acct, Some(status), "http://remote.test/m2.png").await;
    let mid3 = media_row(db, acct, Some(status), "http://remote.test/m3.png").await;
    let kept = fetched("d.png").await;
    sqlx::query("UPDATE media_attachments SET file_path = $2 WHERE id = $1")
        .bind(mid2)
        .bind(&kept.key)
        .execute(db)
        .await
        .unwrap();
    // A single-connection pool makes `lock_timeout` deterministic —
    // SET applies to every query the pool runs.
    let lock_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(env.state.db.connect_options().as_ref().clone())
        .await
        .unwrap();
    sqlx::query("SET lock_timeout = 200")
        .execute(&lock_pool)
        .await
        .unwrap();
    let ctx2 = JobContext {
        db: lock_pool.clone(),
        redis: ctx.redis.clone(),
        config: ctx.config.clone(),
        media: ctx.media.clone(),
    };
    let mut lock_conn = env.state.db.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *lock_conn).await.unwrap();
    sqlx::query("LOCK TABLE media_attachments IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *lock_conn)
        .await
        .unwrap();

    // The UPDATE errored but the row already references this key —
    // the commit landed and the object is kept, not deleted.
    assert!(persist_remote_media(&ctx2, mid2, &kept).await.is_err());
    assert!(
        env.state.media.get(&kept.key).await.is_ok(),
        "referenced object must survive a failed-looking write-back"
    );

    // The UPDATE errored and the row's file_path is NULL — the object
    // is provably unreferenced, so it is dropped.
    let unref = fetched("e.png").await;
    assert!(persist_remote_media(&ctx2, mid3, &unref).await.is_err());
    assert!(env.state.media.get(&unref.key).await.is_err());

    drop(lock_conn);

    // The re-read fails too (pool closed): the outcome is ambiguous,
    // so the object is left — a recoverable leak beats a permanently
    // broken file_path.
    let ambiguous = fetched("f.png").await;
    lock_pool.close().await;
    assert!(persist_remote_media(&ctx2, mid3, &ambiguous).await.is_err());
    assert!(env.state.media.get(&ambiguous.key).await.is_ok());
}

/// A failed enqueue must release the throttle flag — otherwise every
/// later read stays suppressed until the TTL expires. The stream is
/// blocked by writing a String where XADD expects a stream (WRONGTYPE),
/// which fails the enqueue while Redis itself stays up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_profile_refresh_flag_cleared_on_enqueue_failure() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app_federated(Config::default()).await else {
        return;
    };
    let remote = remote_account(&env.state.db, "http://remote.test/u/r").await;
    let flag = env.state.redis.key("actor-refresh:http://remote.test/u/r");
    let stream = env.state.redis.key("queue:default");
    let mut conn = env
        .state
        .redis
        .client()
        .get_multiplexed_async_connection()
        .await
        .unwrap();

    redis::cmd("SET")
        .arg(&stream)
        .arg("blocked")
        .query_async::<()>(&mut conn)
        .await
        .unwrap();
    let (code, entity) = get_json(&env.app, &format!("/api/v1/accounts/{remote}")).await;
    assert_eq!(code, 200, "{entity}");
    let held: i64 = redis::cmd("EXISTS")
        .arg(&flag)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(held, 0, "throttle flag survived a failed enqueue");

    // Unblock: the very next read enqueues successfully.
    redis::cmd("DEL")
        .arg(&stream)
        .query_async::<i64>(&mut conn)
        .await
        .unwrap();
    let (code, entity) = get_json(&env.app, &format!("/api/v1/accounts/{remote}")).await;
    assert_eq!(code, 200, "{entity}");
    let jobs = default_queue_jobs(&env).await;
    let fetches = jobs
        .iter()
        .filter(|j| j["kind"] == "remote_actor_fetch")
        .count();
    assert_eq!(fetches, 1, "expected one RemoteActorFetch, got {jobs:?}");
}
