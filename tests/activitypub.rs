//! Federation integration tests: the Postgres `ActivityPubStorage`
//! adapter, actor provisioning and the `MultiActorHandler` plumbing,
//! run against a real Postgres in a testcontainer.
//!
//! Skipped (with a message, never silently green) when no
//! Docker-compatible container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use pubby_sdk::{
    Activity, ActivityPubStorage, FollowRequest, Follower, Interaction, InteractionStatus,
    InteractionType,
};
use redis::AsyncCommands;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::{Redis as RedisImage, REDIS_PORT};
use tower::ServiceExt;
use yonder::activitypub::storage::PgActivityPubStorage;
use yonder::activitypub::{self, actors};
use yonder::cache::{self, Redis};
use yonder::config::{Config, RedisConfig};
use yonder::db;
use yonder::http::AppState;
use yonder::jobs::{Dispatcher, JobContext, JobHandler};

const BASE: &str = "http://127.0.0.1:4000";

fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

async fn test_pool() -> Option<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return None;
    }
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("postgres container");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPool::connect(&url).await.unwrap();
    db::migrate::run(&pool).await.unwrap();
    Some((pool, container))
}

/// Seed a provisioned local actor named `username` and return it.
async fn seed_actor(pool: &PgPool, username: &str) -> actors::LocalActor {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (domain, username, actor_id) \
         VALUES (NULL, $1, '') RETURNING id",
    )
    .bind(username)
    .fetch_one(pool)
    .await
    .unwrap();
    let (_, updated) = actors::provision_local_actor(pool, BASE, id, username)
        .await
        .unwrap();
    assert!(updated, "fresh account should provision");
    actors::load_local_actor(pool, username)
        .await
        .unwrap()
        .unwrap()
}

/// A remote actor row, minimal.
async fn seed_remote(pool: &PgPool, actor_id: &str, username: &str) {
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ('remote.example', $1, $2, $2, 'https://remote.example/inbox') \
         ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(username)
    .bind(actor_id)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0009_adds_federation_columns() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let has_key: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'accounts' AND column_name = 'private_key_pem')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(has_key);
    let has_qa: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'quote_authorizations')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(has_qa);
    let has_processed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns \
         WHERE table_name = 'activities' AND column_name = 'processed_at')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(has_processed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provision_local_actor_sets_identity_and_key() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let actor = seed_actor(&pool, "alice").await;
    assert_eq!(actor.actor_id, format!("{BASE}/ap/users/alice"));
    let pem = actor.private_key_pem.expect("provisioned key");
    assert!(pem.contains("PRIVATE KEY"));
    // Re-provisioning keeps the same key (no silent rekey) and reports
    // the stored identity with `updated=false`.
    let before = pem.clone();
    let (actor_id, updated) = actors::provision_local_actor(&pool, BASE, actor.id, "alice")
        .await
        .unwrap();
    assert!(!updated);
    assert_eq!(actor_id, actor.actor_id);
    let after = actors::load_local_actor(&pool, "alice")
        .await
        .unwrap()
        .unwrap()
        .private_key_pem
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_follower_roundtrip_and_scoping() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let bob = seed_actor(&pool, "bob").await;
    let storage = PgActivityPubStorage::new(pool.clone());
    let remote = "https://remote.example/ap/users/carol";

    for target in [&alice.actor_id, &bob.actor_id] {
        storage
            .store_follower(&Follower {
                actor_id: remote.to_string(),
                inbox: "https://remote.example/inbox".into(),
                shared_inbox: String::new(),
                followed_at: None,
                actor_data: Map::new(),
                target_actor_id: target.clone(),
            })
            .unwrap();
    }

    // Scoped reads don't leak across actors.
    let alice_followers = storage.get_followers(Some(&alice.actor_id)).unwrap();
    assert_eq!(alice_followers.len(), 1);
    assert_eq!(alice_followers[0].target_actor_id, alice.actor_id);
    let all = storage.get_followers(None).unwrap();
    assert_eq!(all.len(), 2);

    // Undo Follow for one target leaves the other intact.
    storage.remove_follower(remote, &alice.actor_id).unwrap();
    assert_eq!(
        storage.get_followers(Some(&alice.actor_id)).unwrap().len(),
        0
    );
    assert_eq!(storage.get_followers(Some(&bob.actor_id)).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_get_following_returns_accepted_outbound() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let remote = "https://remote.example/ap/users/dave";
    seed_remote(&pool, remote, "dave").await;
    let storage = PgActivityPubStorage::new(pool.clone());

    assert!(storage
        .get_following(&alice.actor_id, 10, 0)
        .unwrap()
        .is_empty());
    // A local→remote follows row = accepted outbound follow.
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id) \
         SELECT la.id, ra.id FROM accounts la, accounts ra \
         WHERE la.actor_id = $1 AND ra.actor_id = $2",
    )
    .bind(&alice.actor_id)
    .bind(remote)
    .execute(&pool)
    .await
    .unwrap();
    let following = storage.get_following(&alice.actor_id, 10, 0).unwrap();
    assert_eq!(following, vec![remote.to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_follow_request_lifecycle() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let storage = PgActivityPubStorage::new(pool.clone());
    let remote = "https://remote.example/ap/users/erin";

    let request = FollowRequest {
        actor_id: remote.to_string(),
        target_actor_id: alice.actor_id.clone(),
        inbox: "https://remote.example/inbox".into(),
        shared_inbox: String::new(),
        actor_data: Map::new(),
        activity: json!({"type": "Follow", "actor": remote})
            .as_object()
            .unwrap()
            .clone(),
        requested_at: None,
    };
    storage.store_follow_request(&request).unwrap();
    let pending = storage.get_follow_requests(Some(&alice.actor_id)).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].actor_id, remote);
    assert!(storage
        .get_follow_request(remote, &alice.actor_id)
        .unwrap()
        .is_some());
    assert!(storage
        .remove_follow_request(remote, &alice.actor_id)
        .unwrap());
    assert!(storage
        .get_follow_requests(Some(&alice.actor_id))
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_activities_scoping() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let bob = seed_actor(&pool, "bob").await;
    let storage = PgActivityPubStorage::new(pool.clone());

    storage
        .store_activity(
            &format!("{}/activities/1", alice.actor_id),
            &json!({"type": "Create", "actor": alice.actor_id, "object": {"id": "o1"}}),
        )
        .unwrap();
    storage
        .store_activity(
            &format!("{}/activities/2", bob.actor_id),
            &json!({"type": "Like", "actor": bob.actor_id}),
        )
        .unwrap();

    let all = storage.get_activities(50, 0).unwrap();
    assert_eq!(all.len(), 2);
    let alice_acts = storage
        .get_activities_for_actor(&alice.actor_id, 50, 0)
        .unwrap();
    assert_eq!(alice_acts.len(), 1);
    assert_eq!(alice_acts[0]["type"], "Create");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_interactions_and_mentions() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let storage = PgActivityPubStorage::new(pool.clone());

    let mut i = Interaction::new(
        "https://remote.example/ap/users/fred",
        &alice.actor_id,
        InteractionType::Like,
    );
    i.object_id = "https://remote.example/objects/1".into();
    i.mentioned_actors = vec![alice.actor_id.clone()];
    storage.store_interaction(&i).unwrap();

    let likes = storage
        .get_interactions(
            &alice.actor_id,
            Some(InteractionType::Like),
            InteractionStatus::Confirmed,
        )
        .unwrap();
    assert_eq!(likes.len(), 1);
    assert_eq!(likes[0].mentioned_actors, vec![alice.actor_id.clone()]);

    let mentioned = storage
        .get_interactions_mentioning(&alice.actor_id, None, InteractionStatus::Confirmed)
        .unwrap();
    assert_eq!(mentioned.len(), 1);

    storage
        .delete_interaction(&i.source_actor_id, &alice.actor_id, InteractionType::Like)
        .unwrap();
    assert!(storage
        .get_interactions(
            &alice.actor_id,
            Some(InteractionType::Like),
            InteractionStatus::Confirmed
        )
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_actor_cache_and_quote_authorizations() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let storage = PgActivityPubStorage::new(pool.clone());

    let doc = json!({"id": "https://remote.example/ap/users/greg", "type": "Person"});
    storage
        .cache_remote_actor("https://remote.example/ap/users/greg", &doc, None)
        .unwrap();
    let cached = storage
        .get_cached_actor("https://remote.example/ap/users/greg", 3600.0)
        .unwrap()
        .expect("fresh cache entry");
    assert_eq!(cached["type"], "Person");
    // A zero max-age always misses.
    assert!(storage
        .get_cached_actor("https://remote.example/ap/users/greg", 0.0)
        .unwrap()
        .is_none());

    let auth = json!({"type": "QuoteAuthorization", "id": "auth-1"});
    storage.store_quote_authorization("auth-1", &auth).unwrap();
    assert_eq!(
        storage.get_quote_authorization("auth-1").unwrap().unwrap(),
        auth
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_instance_actor_is_idempotent() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let first = actors::ensure_instance_actor(&pool, BASE).await.unwrap();
    assert_eq!(first.username, actors::INSTANCE_USERNAME);
    assert!(first.private_key_pem.is_some());
    let second = actors::ensure_instance_actor(&pool, BASE).await.unwrap();
    assert_eq!(first.private_key_pem, second.private_key_pem);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_doc_advertises_per_actor_endpoints() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let storage: Arc<dyn ActivityPubStorage> = Arc::new(PgActivityPubStorage::new(pool.clone()));
    let handler = actors::handler_for(
        storage,
        &Config::default(),
        BASE,
        &alice,
        &actors::actor_path("alice"),
        pubby_sdk::handlers::HandlerOptions::default(),
    )
    .unwrap()
    .expect("handler for provisioned actor");
    let doc: Value = handler.get_actor_document();
    assert_eq!(doc["id"], format!("{BASE}/ap/users/alice"));
    assert_eq!(doc["inbox"], format!("{BASE}/ap/users/alice/inbox"));
    assert_eq!(doc["outbox"], format!("{BASE}/ap/users/alice/outbox"));
    assert_eq!(doc["followers"], format!("{BASE}/ap/users/alice/followers"));
    assert_eq!(doc["following"], format!("{BASE}/ap/users/alice/following"));
    assert_eq!(doc["endpoints"]["sharedInbox"], format!("{BASE}/ap/inbox"));
    assert_eq!(
        doc["publicKey"]["id"],
        format!("{BASE}/ap/users/alice#main-key")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moderation_domain_block_merge() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('evil.example', 'suspend')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('quiet.example', 'silence')")
        .execute(&pool)
        .await
        .unwrap();
    let config = yonder::config::Config::default();
    let blocked = yonder::activitypub::moderation::blocked_domains(&pool, &config)
        .await
        .unwrap();
    assert_eq!(blocked, vec!["evil.example".to_string()]);
    assert!(
        yonder::activitypub::moderation::is_blocked(&pool, "evil.example")
            .await
            .unwrap()
    );
    assert!(
        !yonder::activitypub::moderation::is_blocked(&pool, "quiet.example")
            .await
            .unwrap()
    );
}

// ---------- Route / worker / on_activity coverage ----------

struct Containers {
    _pg: ContainerAsync<Postgres>,
    _redis: ContainerAsync<RedisImage>,
}

/// Postgres + Redis containers, migrated schema, a built `ApContext`
/// and the full `AppState` — the pieces the route and job tests need.
async fn test_state() -> Option<(Arc<AppState>, Containers)> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return None;
    }
    let pg = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("postgres container");
    let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
    let pool = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
    ))
    .await
    .unwrap();
    db::migrate::run(&pool).await.unwrap();

    let rc = RedisImage::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("redis container");
    let redis_port = rc.get_host_port_ipv4(REDIS_PORT).await.unwrap();
    let redis = cache::connect(&RedisConfig {
        url: format!("redis://127.0.0.1:{redis_port}/0"),
        ..Default::default()
    })
    .await
    .unwrap();

    let mut config = Config::default();
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    let dispatcher = Dispatcher::new(&redis, &config.jobs);
    let mut state = AppState::new(config, pool, redis);
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
    Some((
        Arc::new(state),
        Containers {
            _pg: pg,
            _redis: rc,
        },
    ))
}

fn job_ctx(state: &AppState) -> JobContext {
    JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    }
}

fn ap_router(state: &Arc<AppState>) -> axum::Router {
    activitypub::router(state).with_state(state.clone())
}

async fn post(app: &axum::Router, path: &str, body: Vec<u8>) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/activity+json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn stream_len(redis: &Redis, queue: &str) -> usize {
    let mut conn = redis.conn().await.unwrap();
    conn.xlen(redis.key(&format!("queue:{queue}")))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_route_validates_dedupes_and_blocks() {
    let Some((state, _containers)) = test_state().await else {
        return;
    };
    let app = ap_router(&state);
    let alice = seed_actor(&state.db, "alice").await;
    sqlx::query("INSERT INTO domain_blocks (domain, severity) VALUES ('evil.example', 'suspend')")
        .execute(&state.db)
        .await
        .unwrap();

    let activity = json!({
        "type": "Follow",
        "id": "https://remote.example/activities/f1",
        "actor": "https://remote.example/users/fred",
        "object": alice.actor_id,
    });
    let body = serde_json::to_vec(&activity).unwrap();

    // Valid → 202, dedupe row, one queued job.
    let resp = post(&app, "/ap/inbox", body.clone()).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let seen: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM activities WHERE activity_id = $1 AND NOT local)",
    )
    .bind("https://remote.example/activities/f1")
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert!(seen);
    assert_eq!(stream_len(&state.redis, "federation").await, 1);

    // Same id again → 202 without a second job or row.
    let resp = post(&app, "/ap/inbox", body).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(stream_len(&state.redis, "federation").await, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM activities WHERE activity_id = $1")
        .bind("https://remote.example/activities/f1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Per-user inbox route also accepts.
    let per_user = json!({
        "type": "Like",
        "id": "https://remote.example/activities/l1",
        "actor": "https://remote.example/users/fred",
        "object": format!("{}/statuses/1", alice.actor_id),
    });
    let resp = post(
        &app,
        "/ap/users/alice/inbox",
        serde_json::to_vec(&per_user).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(stream_len(&state.redis, "federation").await, 2);

    // Suspended domain, single-string actor → 403 at the edge.
    let blocked = json!({
        "type": "Follow",
        "id": "https://evil.example/activities/b1",
        "actor": "https://evil.example/users/mallory",
        "object": alice.actor_id,
    });
    let resp = post(&app, "/ap/inbox", serde_json::to_vec(&blocked).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // …and in array form (the D1 regression — previously skipped the
    // check entirely).
    let blocked_array = json!({
        "type": "Follow",
        "id": "https://ok.example/activities/b2",
        "actor": ["https://ok.example/users/a", {"id": "https://evil.example/users/b"}],
        "object": alice.actor_id,
    });
    let resp = post(
        &app,
        "/ap/inbox",
        serde_json::to_vec(&blocked_array).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Malformed JSON → 400.
    let resp = post(&app, "/ap/inbox", b"not json".to_vec()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Over the 8 MiB cap → 413.
    let resp = post(&app, "/ap/inbox", vec![b' '; 8 * 1024 * 1024 + 1]).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_signature_failure_releases_dedupe_row() {
    let Some((state, _containers)) = test_state().await else {
        return;
    };
    let alice = seed_actor(&state.db, "alice").await;
    let ap = state.ap.clone().unwrap();

    // What the inbox route would have written for this activity.
    let raw = json!({
        "type": "Follow",
        "id": "https://remote.example/activities/f9",
        "actor": "https://remote.example/users/fred",
        "object": alice.actor_id,
    });
    sqlx::query(
        "INSERT INTO activities (activity_id, type, actor_id, object, local) \
         VALUES ($1,'Follow',$2,$3,false)",
    )
    .bind("https://remote.example/activities/f9")
    .bind("https://remote.example/users/fred")
    .bind(&raw)
    .execute(&state.db)
    .await
    .unwrap();

    let payload = json!({
        "activity": raw,
        "activity_id": "https://remote.example/activities/f9",
        "username": "alice",
        "path": "/ap/users/alice/inbox",
        "headers": {}, // no Signature → verification fails
        "body_hex": "",
    });
    let job = activitypub::incoming::FederationIncomingJob::new(ap);
    job.run(&job_ctx(&state), &payload).await.unwrap();

    // The dedupe claim is released so a correctly-signed redelivery
    // can be processed later.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM activities WHERE activity_id = $1)")
            .bind("https://remote.example/activities/f9")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(!exists);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_flag_reports_resolvable_objects() {
    let Some((state, _c)) = test_state().await else {
        return;
    };
    let pool = &state.db;
    let alice = seed_actor(pool, "alice").await;
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, uri, content) \
         SELECT id, $2, 'a post' FROM accounts WHERE id = $1 RETURNING id",
    )
    .bind(alice.id)
    .bind(format!("{}/statuses/1", alice.actor_id))
    .fetch_one(pool)
    .await
    .unwrap();
    let status_uri = format!("{}/statuses/1", alice.actor_id);
    let reporter = "https://remote.example/users/fred";

    let raw = json!({
        "type": "Flag",
        "id": "https://remote.example/activities/flag1",
        "actor": reporter,
        "content": "spam ".repeat(2000), // 10k chars → truncated to the config cap (2000)
        "object": [
            alice.actor_id,                  // account → target_account_id
            status_uri,                      // status → status_id + author
            "https://remote.example/o/404",  // unresolvable → skipped
        ],
    });
    let activity = Activity::from_json(&raw);
    let core = state.ap.as_ref().unwrap().core.clone();
    activitypub::incoming::handle_flag(&core, &activity, &raw)
        .await
        .unwrap();

    // Two reports: one account-targeted, one status-targeted. The
    // unresolvable URL produced neither a report nor an accounts row.
    let reports: Vec<(i64, Option<i64>, i64)> = sqlx::query_as(
        "SELECT target_account_id, status_id, char_length(comment)::bigint \
         FROM reports ORDER BY status_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0], (alice.id, None, 2000));
    assert_eq!(reports[1], (alice.id, Some(status_id), 2000));

    let fabricated: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1)")
            .bind("https://remote.example/o/404")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(!fabricated, "status URI must not become an accounts row");

    // The reporter (a real actor) was upserted as the reporting account.
    let reporter_row: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1)")
            .bind(reporter)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(reporter_row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_move_enqueues_verification_and_writes_nothing() {
    let Some((state, _c)) = test_state().await else {
        return;
    };
    let alice = seed_actor(&state.db, "alice").await;
    let core = state.ap.as_ref().unwrap().core.clone();

    // An actor claiming to move onto a local account defers to
    // MigrateFollowers — verification (target alsoKnownAs) and all
    // writes happen in the job, never inline.
    let raw = json!({
        "type": "Move",
        "actor": "https://remote.example/users/mover",
        "object": "https://remote.example/users/mover",
        "target": alice.actor_id,
    });
    let activity = Activity::from_json(&raw);
    activitypub::incoming::handle_move(&core, &activity, &raw)
        .await
        .unwrap();
    assert_eq!(stream_len(&state.redis, "bulk").await, 1);

    // No rows may be written before the target's alsoKnownAs is
    // verified — in particular no alias letting a foreign actor pose
    // as a local account's old address.
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1)"
    )
    .bind("https://remote.example/users/mover")
    .fetch_one(&state.db)
    .await
    .unwrap());
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM account_aliases WHERE account_id = $1)"
    )
    .bind(alice.id)
    .fetch_one(&state.db)
    .await
    .unwrap());

    // Self-moves are dropped before enqueueing.
    let raw = json!({
        "type": "Move",
        "actor": "https://remote.example/users/self",
        "object": "https://remote.example/users/self",
        "target": "https://remote.example/users/self",
    });
    let activity = Activity::from_json(&raw);
    activitypub::incoming::handle_move(&core, &activity, &raw)
        .await
        .unwrap();
    assert_eq!(stream_len(&state.redis, "bulk").await, 1);

    // A move where `object` differs from `actor` (moving someone else)
    // is dropped too.
    let raw = json!({
        "type": "Move",
        "actor": "https://remote.example/users/attacker",
        "object": "https://remote.example/users/victim",
        "target": alice.actor_id,
    });
    let activity = Activity::from_json(&raw);
    activitypub::incoming::handle_move(&core, &activity, &raw)
        .await
        .unwrap();
    assert_eq!(stream_len(&state.redis, "bulk").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_unhandled_records_interaction() {
    let Some((pool, _c)) = test_pool().await else {
        return;
    };
    let alice = seed_actor(&pool, "alice").await;
    let storage = PgActivityPubStorage::new(pool.clone());
    let raw = json!({
        "type": "Block",
        "id": "https://remote.example/activities/b9",
        "actor": "https://remote.example/users/fred",
        "object": alice.actor_id,
    });
    let activity = Activity::from_json(&raw);
    activitypub::incoming::store_unhandled(&storage, &activity, &raw)
        .await
        .unwrap();
    let rows = storage
        .get_interactions(
            &alice.actor_id,
            Some(InteractionType::Mention),
            InteractionStatus::Confirmed,
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].object_id, alice.actor_id);
    assert_eq!(rows[0].metadata["ap_type"], "Block");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delivery_sweep_requeues_only_stale_pending() {
    let Some((state, _containers)) = test_state().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO activities (activity_id, type, actor_id, object, local) \
         VALUES ('a1','Create','x','{}'::jsonb,true)",
    )
    .execute(&state.db)
    .await
    .unwrap();
    // One stale pending row (its job was lost) and one fresh one.
    sqlx::query(
        "INSERT INTO deliveries (activity_id, inbox_url, next_attempt_at) \
         SELECT id, 'https://remote.example/inbox', now() - interval '1 minute' \
         FROM activities WHERE activity_id = 'a1'",
    )
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deliveries (activity_id, inbox_url, next_attempt_at) \
         SELECT id, 'https://remote.example/other', now() + interval '1 hour' \
         FROM activities WHERE activity_id = 'a1'",
    )
    .execute(&state.db)
    .await
    .unwrap();

    activitypub::delivery::FederationDeliverySweepJob
        .run(&job_ctx(&state), &json!({}))
        .await
        .unwrap();

    // The stale row got a new job + a fresh deadline; the fresh row
    // was left alone.
    assert_eq!(stream_len(&state.redis, "federation").await, 1);
    let stale_future: bool = sqlx::query_scalar(
        "SELECT next_attempt_at > now() FROM deliveries \
         WHERE inbox_url = 'https://remote.example/inbox'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert!(stale_future);
    let fresh_unchanged: bool = sqlx::query_scalar(
        "SELECT next_attempt_at > now() + interval '30 minutes' FROM deliveries \
         WHERE inbox_url = 'https://remote.example/other'",
    )
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert!(fresh_unchanged);
}
