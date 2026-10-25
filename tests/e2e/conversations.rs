//! Epic-25 direct-message conversations: grouped listing with unread
//! state, thread pagination, caller-only read/hide markers, key
//! maintenance across local/remote/imported writes, and the
//! Rust-vs-SQL key parity pin.
//!
//! Skipped when no container runtime is reachable (same heuristic as
//! `tests/db.rs`).

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    posts::{self, remote},
    users::transfer,
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
    config.rate_limit.timeline = config.rate_limit.authentication.clone();
    config.rate_limit.relation = config.rate_limit.authentication.clone();
    config.rate_limit.search = config.rate_limit.authentication.clone();
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-conv-{}", uuid::Uuid::new_v4()))
        .display()
        .to_string();
    config.media.backend = yonder::config::MediaBackend::Local;

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
) -> (StatusCode, axum::http::HeaderMap, Value) {
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
    let json = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, headers, json)
}

/// Post a status, return its entity (id as string).
async fn post(app: &Router, token: &str, body: Value) -> Value {
    let mut payload = json!({"status": "hello"});
    if let Value::Object(m) = body {
        for (k, v) in m {
            payload[k] = v;
        }
    }
    let (status, _, entity) =
        request(app, "POST", "/api/v1/statuses", Some(token), Some(payload)).await;
    assert_eq!(status, StatusCode::OK, "post failed: {entity}");
    entity
}

async fn account_id(db: &sqlx::PgPool, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
}

/// `GET /api/v1/conversations` for `token` → the raw conversation array.
async fn conversation_list(app: &Router, token: &str) -> Vec<Value> {
    let (status, _, body) = request(app, "GET", "/api/v1/conversations", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().clone()
}

/// Direct `status_mentions` writers beyond `mentions::persist` — kept
/// greppable in the module doc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_exchange_groups_and_read_marker() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;

    let first = post(
        &env.app,
        &alice,
        json!({"status": "@bob hi there", "visibility": "direct"}),
    )
    .await;
    assert_eq!(first["visibility"], "direct");
    // Bob replies — the carried-over mention keeps the key stable.
    let reply = post(
        &env.app,
        &bob,
        json!({"status": "@alice hey!", "visibility": "direct", "in_reply_to_id": first["id"]}),
    )
    .await;

    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs.len(), 1, "{convs:?}");
    let conv = &convs[0];
    assert_eq!(conv["last_status"]["id"], reply["id"]);
    assert_eq!(conv["unread"], true, "reply makes it unread for alice");
    let accounts = conv["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["acct"], "bob");

    // Bob already read it (he wrote the newest status) — not unread.
    let bob_convs = conversation_list(&env.app, &bob).await;
    assert_eq!(bob_convs.len(), 1);
    assert_eq!(bob_convs[0]["unread"], false);

    // Marking read flips alice's unread.
    let key = conv["id"].as_str().unwrap();
    let (status, _, marked) = request(
        &env.app,
        "POST",
        &format!("/api/v1/conversations/{key}/read"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{marked}");
    assert_eq!(marked["unread"], false);
    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs[0]["unread"], false);

    // Thread: both statuses, newest first.
    let (status, _, thread) = request(
        &env.app,
        "GET",
        &format!("/api/v1/conversations/{key}/statuses"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{thread}");
    let ids: Vec<&str> = thread
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![reply["id"].as_str().unwrap(), first["id"].as_str().unwrap()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_hides_for_caller_only_and_revives() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;
    post(
        &env.app,
        &alice,
        json!({"status": "@bob secret", "visibility": "direct"}),
    )
    .await;
    let key = conversation_list(&env.app, &bob).await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Bob hides it — caller-only: the statuses survive and alice's
    // list is untouched.
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/conversations/{key}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(conversation_list(&env.app, &bob).await.is_empty());
    assert_eq!(conversation_list(&env.app, &alice).await.len(), 1);

    // A newer reply revives it for bob (and only him).
    let convs = conversation_list(&env.app, &alice).await;
    post(
        &env.app,
        &alice,
        json!({
            "status": "@bob back again", "visibility": "direct",
            "in_reply_to_id": convs[0]["last_status"]["id"],
        }),
    )
    .await;
    let bob_convs = conversation_list(&env.app, &bob).await;
    assert_eq!(bob_convs.len(), 1, "{bob_convs:?}");
    assert_eq!(bob_convs[0]["id"], key, "same participant set, same key");
    assert_eq!(bob_convs[0]["unread"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_participant_and_malformed_keys_404() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;
    let eve = register(&env.app, "eve", "eve@x.com").await;
    post(
        &env.app,
        &alice,
        json!({"status": "@bob secret", "visibility": "direct"}),
    )
    .await;
    let key = conversation_list(&env.app, &bob).await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Eve is not a participant — a *valid, real* key still 404s on
    // every operation (the key grants no access on its own).
    for (method, path) in [
        ("GET", format!("/api/v1/conversations/{key}/statuses")),
        ("POST", format!("/api/v1/conversations/{key}/read")),
        ("DELETE", format!("/api/v1/conversations/{key}")),
    ] {
        let (status, _, body) = request(&env.app, method, &path, Some(&eve), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {body}");
    }
    assert!(conversation_list(&env.app, &eve).await.is_empty());

    // Malformed keys — same 404, before any DB access.
    for bad in [
        "notakey",
        &"a".repeat(63),
        &"A".repeat(64),                 // uppercase hex is malformed
        &format!("{}g", "a".repeat(63)), // non-hex character
    ] {
        let (status, _, body) = request(
            &env.app,
            "GET",
            &format!("/api/v1/conversations/{bad}/statuses"),
            Some(&alice),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}: {body}");
    }
    // Unknown-but-wellformed key 404s too.
    let (status, _, _) = request(
        &env.app,
        "GET",
        &format!("/api/v1/conversations/{}/statuses", "f".repeat(64)),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_participant_and_self_mention() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    register(&env.app, "bob", "bob@x.com").await;
    let carol = register(&env.app, "carol", "carol@x.com").await;

    // A two-party thread and a group thread are different keys.
    post(
        &env.app,
        &alice,
        json!({"status": "@bob one", "visibility": "direct"}),
    )
    .await;
    post(
        &env.app,
        &alice,
        json!({"status": "@bob @carol group", "visibility": "direct"}),
    )
    .await;
    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs.len(), 2, "{convs:?}");
    let group = convs
        .iter()
        .find(|c| c["accounts"].as_array().unwrap().len() == 2)
        .expect("group conversation");
    let group_accts: Vec<&str> = group["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["acct"].as_str().unwrap())
        .collect();
    assert!(group_accts.contains(&"bob") && group_accts.contains(&"carol"));
    // Carol sees only the group thread.
    let carol_convs = conversation_list(&env.app, &carol).await;
    assert_eq!(carol_convs.len(), 1);
    assert_eq!(carol_convs[0]["id"], group["id"]);

    // A self-mention does not widen the participant set — still the
    // alice/bob conversation, not a third one.
    post(
        &env.app,
        &alice,
        json!({"status": "@alice @bob note to self", "visibility": "direct"}),
    )
    .await;
    assert_eq!(conversation_list(&env.app, &alice).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_moves_key_and_visibility_change_clears() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let bob = register(&env.app, "bob", "bob@x.com").await;
    register(&env.app, "carol", "carol@x.com").await;

    let dm = post(
        &env.app,
        &alice,
        json!({"status": "@bob first", "visibility": "direct"}),
    )
    .await;
    let original_key: String =
        sqlx::query_scalar("SELECT conversation_key FROM statuses WHERE id = $1")
            .bind(dm["id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&env.state.db)
            .await
            .unwrap();

    // Edit to a different mention set → the status moves keys.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{}", dm["id"].as_str().unwrap()),
        Some(&alice),
        Some(json!({"status": "@carol moved", "visibility": "direct"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let moved_key: String =
        sqlx::query_scalar("SELECT conversation_key FROM statuses WHERE id = $1")
            .bind(dm["id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_ne!(moved_key, original_key, "edit must recompute the key");
    // Alice now sees the alice/carol conversation; the alice/bob one is empty.
    let alice_convs = conversation_list(&env.app, &alice).await;
    assert_eq!(alice_convs.len(), 1);
    assert_eq!(alice_convs[0]["id"].as_str().unwrap(), moved_key);
    assert_eq!(
        alice_convs[0]["accounts"].as_array().unwrap()[0]["acct"],
        "carol"
    );
    assert!(conversation_list(&env.app, &bob).await.is_empty());

    // Editing the visibility away from direct clears the key.
    let (status, _, _) = request(
        &env.app,
        "PUT",
        &format!("/api/v1/statuses/{}", dm["id"].as_str().unwrap()),
        Some(&alice),
        Some(json!({"status": "@carol now public", "visibility": "public"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cleared: Option<String> =
        sqlx::query_scalar("SELECT conversation_key FROM statuses WHERE id = $1")
            .bind(dm["id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(cleared.is_none(), "non-direct must clear the key");
    assert!(conversation_list(&env.app, &alice).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_dm_ingest_keys_and_groups_with_reply() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    let alice_actor = format!("{BASE}/ap/users/alice");
    let core = env.state.ap.as_ref().unwrap().core.clone();

    // Inbound remote DM to alice (to = her actor URL, no Public).
    let activity = pubby_sdk::Activity {
        id: "https://remote.test/activities/1".into(),
        activity_type: "Create".into(),
        actor: "https://remote.test/users/eve".into(),
        object: json!({
            "id": "https://remote.test/objects/dm1",
            "type": "Note",
            "attributedTo": "https://remote.test/users/eve",
            "content": "<p>hi alice</p>",
            "to": [alice_actor],
            "tag": [{"type": "Mention", "href": alice_actor, "name": "@alice@127.0.0.1:4000"}],
            "published": "2025-01-01T00:00:00Z",
        }),
        to: vec![alice_actor.clone()],
        cc: vec![],
        published: None,
        signature: None,
    };
    assert!(remote::handle(&core, &activity, &activity.to_json()).await);
    let key: Option<String> = sqlx::query_scalar(
        "SELECT conversation_key FROM statuses WHERE uri = 'https://remote.test/objects/dm1'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let key = key.expect("remote DM gets a key");

    // Alice sees the conversation with eve as the other participant.
    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs.len(), 1, "{convs:?}");
    assert_eq!(convs[0]["id"].as_str().unwrap(), key);
    assert_eq!(convs[0]["unread"], true);
    assert_eq!(
        convs[0]["accounts"].as_array().unwrap()[0]["acct"],
        "eve@remote.test"
    );

    // Alice's local reply lands in the same conversation.
    post(
        &env.app,
        &alice,
        json!({
            "status": "@eve@remote.test hello back", "visibility": "direct",
            "in_reply_to_id": convs[0]["last_status"]["id"],
        }),
    )
    .await;
    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs.len(), 1, "{convs:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_dm_gets_key() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let _alice = register(&env.app, "alice", "alice@x.com").await;
    let alice = account_id(&env.state.db, "alice").await;
    let alice_actor = format!("{BASE}/ap/users/alice");

    // An archive outbox with one direct Create mentioning alice.
    let outbox = json!({
        "type": "OrderedCollection",
        "orderedItems": [{
            "type": "Create",
            "actor": format!("{BASE}/ap/users/alice"),
            "object": {
                "id": "https://old.example/statuses/700",
                "type": "Note",
                "attributedTo": alice_actor,
                "content": "old dm",
                "to": ["https://remote.test/users/pal"],
                "tag": [{"type": "Mention", "href": "https://remote.test/users/pal"}],
                "published": "2020-05-05T05:05:05Z",
            },
        }],
    });
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut zip_buf);
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("outbox.json", opts).unwrap();
        std::io::Write::write_all(&mut w, outbox.to_string().as_bytes()).unwrap();
        w.finish().unwrap();
    }
    let zkey = format!("imports/{alice}/dms.zip");
    env.state
        .media
        .put(&zkey, &zip_buf.into_inner(), "application/zip")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key) \
         VALUES ($1, 'archive', 'merge', $2) RETURNING id",
    )
    .bind(alice)
    .bind(&zkey)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(
            &JobContext {
                db: env.state.db.clone(),
                redis: env.state.redis.clone(),
                config: env.state.config().clone(),
                media: env.state.media.clone(),
            },
            &json!({ "import_id": import_id }),
        )
        .await
        .unwrap();
    let (vis, key): (String, Option<String>) = sqlx::query_as(
        "SELECT visibility, conversation_key FROM statuses \
         WHERE account_id = $1 AND original_id = 'https://old.example/statuses/700'",
    )
    .bind(alice)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(vis, "direct");
    assert!(key.is_some(), "imported DM must get a conversation key");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstoned_last_status_falls_back() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    register(&env.app, "bob", "bob@x.com").await;
    let first = post(
        &env.app,
        &alice,
        json!({"status": "@bob one", "visibility": "direct"}),
    )
    .await;
    let second = post(
        &env.app,
        &alice,
        json!({
            "status": "@bob two", "visibility": "direct",
            "in_reply_to_id": first["id"],
        }),
    )
    .await;
    // Tombstone the newest — the conversation falls back to `first`.
    let (status, _, _) = request(
        &env.app,
        "DELETE",
        &format!("/api/v1/statuses/{}", second["id"].as_str().unwrap()),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let convs = conversation_list(&env.app, &alice).await;
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0]["last_status"]["id"], first["id"]);
    let (status, _, thread) = request(
        &env.app,
        "GET",
        &format!(
            "/api/v1/conversations/{}/statuses",
            convs[0]["id"].as_str().unwrap()
        ),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(thread.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grouped_pagination_and_thread_pagination() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    // Three distinct exchanges → three conversations.
    for name in ["u1", "u2", "u3"] {
        register(&env.app, name, &format!("{name}@x.com")).await;
        post(
            &env.app,
            &alice,
            json!({"status": format!("@{name} hi"), "visibility": "direct"}),
        )
        .await;
    }
    // limit=2: first page has exactly two conversations, no repeats.
    let (status, headers, page1) = request(
        &env.app,
        "GET",
        "/api/v1/conversations?limit=2",
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1.as_array().unwrap().len(), 2);
    let link = headers
        .get(header::LINK)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(link.contains("rel=\"next\""), "missing next: {link}");
    let max_id = link
        .split("max_id=")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .and_then(|s| s.split('>').next())
        .unwrap()
        .to_string();
    let (status, _, page2) = request(
        &env.app,
        "GET",
        &format!("/api/v1/conversations?limit=2&max_id={max_id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let page2 = page2.as_array().unwrap();
    assert_eq!(page2.len(), 1);
    // No conversation appears on both pages.
    let ids1: Vec<&str> = page1
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(!ids1.contains(&page2[0]["id"].as_str().unwrap()));

    // Thread pagination: several messages, each returned once.
    let bob = register(&env.app, "bob", "bob@x.com").await;
    let mut last = post(
        &env.app,
        &alice,
        json!({"status": "@bob m1", "visibility": "direct"}),
    )
    .await;
    for i in 2..=4 {
        last = post(
            &env.app,
            &alice,
            json!({
                "status": format!("@bob m{i}"), "visibility": "direct",
                "in_reply_to_id": last["id"],
            }),
        )
        .await;
    }
    let key = conversation_list(&env.app, &bob).await[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = String::new();
    for _ in 0..4 {
        let (status, headers, page) = request(
            &env.app,
            "GET",
            &format!("/api/v1/conversations/{key}/statuses?limit=2{cursor}"),
            Some(&bob),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let arr = page.as_array().unwrap();
        for s in arr {
            seen.push(s["id"].as_str().unwrap().to_string());
        }
        let link = headers
            .get(header::LINK)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let Some(next) = link
            .split("max_id=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .and_then(|s| s.split('>').next())
        else {
            break;
        };
        cursor = format!("&max_id={next}");
    }
    assert_eq!(seen.len(), 4, "{seen:?}");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(seen.len(), unique.len(), "thread pages must not repeat");
}

/// The migration-0023 backfill expression and the runtime helper must
/// produce byte-identical keys, or pre-migration threads would split
/// from their post-migration replies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_sql_key_parity() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let alice = register(&env.app, "alice", "alice@x.com").await;
    register(&env.app, "bob", "bob@x.com").await;
    // A mention set with an out-of-order id and a self-mention (the
    // dedupe path): alice (low id) mentions bob (high id) + herself.
    let dm = post(
        &env.app,
        &alice,
        json!({"status": "@alice @bob keyed", "visibility": "direct"}),
    )
    .await;
    let status_id: i64 = dm["id"].as_str().unwrap().parse().unwrap();

    // Rust path: clear the stored key, recompute with the helper.
    sqlx::query("UPDATE statuses SET conversation_key = NULL WHERE id = $1")
        .bind(status_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    posts::conversations::refresh_conversation_key_standalone(&env.state.db, status_id)
        .await
        .unwrap();
    let rust_key: String =
        sqlx::query_scalar("SELECT conversation_key FROM statuses WHERE id = $1")
            .bind(status_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();

    // SQL path: the exact backfill expression from the migration.
    sqlx::query("UPDATE statuses SET conversation_key = NULL WHERE id = $1")
        .bind(status_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE statuses s SET conversation_key = ( \
             SELECT encode(sha256(convert_to( \
                 string_agg(p.account_id::text, ',' ORDER BY p.account_id), 'UTF8')), 'hex') \
             FROM ( \
                 SELECT s.account_id \
                 UNION \
                 SELECT m.account_id FROM status_mentions m WHERE m.status_id = s.id \
             ) p \
         ) WHERE s.visibility = 'direct'",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    let sql_key: String = sqlx::query_scalar("SELECT conversation_key FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(rust_key, sql_key, "Rust and SQL keys must agree");

    // And both equal `participant_key` over the true set.
    let alice_id = account_id(&env.state.db, "alice").await;
    let bob_id = account_id(&env.state.db, "bob").await;
    assert_eq!(
        rust_key,
        posts::conversations::participant_key(&[bob_id, alice_id, alice_id])
    );
}
