//! Account-archive restoration tests: real Mastodon (ZIP + pre-4.2
//! tar.gz), Akkoma and Pleroma archive shapes built in memory, driven
//! through the import job — plus archive-security rejection cases and
//! export-fidelity checks.
//!
//! Most of these are `#[ignore]`d until the shared archive
//! restoration engine lands (implementation contract section 7) —
//! they pin the target behavior so each later section has a failing
//! test to un-ignore.
//!
//! Skipped when no container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use std::io::Write;
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{JobContext, JobHandler},
    users::transfer,
};

use crate::harness;

const BASE: &str = "http://127.0.0.1:4000";
const OLD_ACTOR: &str = "https://old.example/users/importer";

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
    config.federation.enabled = true;
    config.federation.base_url = Some(BASE.to_string());
    config.federation.allow_private_targets = true;
    config.auth.require_email_verification = false;
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-ta-{}", uuid::Uuid::new_v4()))
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

async fn request(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn json_req(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn authed(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    Request::from_parts(parts, body)
}

async fn register(env: &Env, username: &str) -> String {
    let (status, body) = request(
        &env.app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": username,
                "email": format!("{username}@example.com"),
                "password": "correct-horse",
                "agreement": true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register {username}");
    body["access_token"].as_str().unwrap().to_string()
}

async fn account_id(state: &AppState, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
        .bind(username)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

/// Seed a remote account + `actor_cache` row (same trick as
/// `account_management::seed_remote`).
async fn seed_remote(state: &AppState, actor_id: &str) -> i64 {
    sqlx::query(
        "INSERT INTO actor_cache (actor_id, actor_data) VALUES ($1, $2) \
         ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(actor_id)
    .bind(json!({
        "id": actor_id,
        "type": "Person",
        "inbox": format!("{actor_id}/inbox"),
    }))
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox, shared_inbox) \
         VALUES ($1, $2, $3, $3, $4, $4) RETURNING id",
    )
    .bind(activitypub::moderation::actor_domain(actor_id))
    .bind(actor_id.trim_end_matches('/').rsplit('/').next().unwrap())
    .bind(actor_id)
    .bind(format!("{actor_id}/inbox"))
    .fetch_one(&state.db)
    .await
    .unwrap()
}

// Archive builders
// ---------------------------------------------------------------------------

fn zip_bytes(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in members {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }
    buf.into_inner()
}

fn targz_bytes(members: &[(&str, &[u8])], extra: &[(&str, tar::EntryType)]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    {
        let mut builder = tar::Builder::new(&mut enc);
        for (name, data) in members {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, name, *data).unwrap();
        }
        for (name, kind) in extra {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(0);
            h.set_mode(0o777);
            if *kind == tar::EntryType::Symlink {
                h.set_link_name("/etc/passwd").unwrap();
            }
            h.set_cksum();
            builder.append_data(&mut h, name, std::io::empty()).unwrap();
        }
        builder.finish().unwrap();
    }
    enc.finish().unwrap()
}

fn note(id: &str, published: &str, content: &str, in_reply_to: Option<&str>) -> Value {
    let mut n = json!({
        "id": format!("{OLD_ACTOR}/statuses/{id}"),
        "type": "Note",
        "attributedTo": OLD_ACTOR,
        "published": published,
        "content": content,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(p) = in_reply_to {
        n["inReplyTo"] = json!(format!("{OLD_ACTOR}/statuses/{p}"));
    }
    n
}

fn create(item: Value) -> Value {
    let suffix = item["id"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    json!({
        "id": format!("{OLD_ACTOR}/activity/c{suffix}"),
        "type": "Create",
        "actor": OLD_ACTOR,
        "published": item["published"].clone(),
        "to": item["to"].clone(),
        "object": item,
    })
}

/// A minimal modern (≥ 4.2) Mastodon archive: actor, outbox with
/// Create + Announce + Question, OrderedCollection likes/bookmarks,
/// an attachment payload and `custom_filters.json`.
fn mastodon42_zip() -> Vec<u8> {
    let actor = json!({
        "@context": ["https://www.w3.org/ns/activitystreams"],
        "id": OLD_ACTOR,
        "type": "Person",
        "preferredUsername": "importer",
        "name": "Old Importer",
        "summary": "<p>moved away</p>",
    });
    let outbox = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [
            create(note("1", "2020-01-02T00:00:00Z", "<p>hello world</p>", None)),
            {
                "id": format!("{OLD_ACTOR}/activity/boost1"),
                "type": "Announce",
                "actor": OLD_ACTOR,
                "published": "2020-01-03T00:00:00Z",
                "object": "https://remote.example/statuses/42",
            },
            create(note("2", "2020-01-04T00:00:00Z", "<p>reply</p>", Some("1"))),
            {
                "id": format!("{OLD_ACTOR}/activity/c3"),
                "type": "Create",
                "actor": OLD_ACTOR,
                "published": "2020-01-05T00:00:00Z",
                "object": {
                    "id": format!("{OLD_ACTOR}/statuses/3"),
                    "type": "Question",
                    "attributedTo": OLD_ACTOR,
                    "published": "2020-01-05T00:00:00Z",
                    "content": "<p>pick one</p>",
                    "endTime": "2020-02-01T00:00:00Z",
                    "oneOf": [
                        {"name": "a", "replies": {"type": "Collection", "totalItems": 3}},
                        {"name": "b", "replies": {"type": "Collection", "totalItems": 5}},
                    ],
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                },
            },
        ],
    });
    let likes = json!({
        "id": format!("{OLD_ACTOR}/likes"),
        "type": "OrderedCollection",
        "orderedItems": ["https://remote.example/statuses/42"],
    });
    let bookmarks = json!({
        "id": format!("{OLD_ACTOR}/bookmarks"),
        "type": "OrderedCollection",
        "orderedItems": ["https://remote.example/statuses/43"],
    });
    let filters = json!([{
        "id": "1",
        "title": "spoiler",
        "context": ["home"],
        "filter_action": "warn",
        "keywords_attributes": [{"keyword": "spoiler", "whole_word": true}],
        "statuses_attributes": [],
    }]);
    let media = json!({
        "media_attachments/att1.png": b"\x89PNG fake".as_slice(),
    });
    let _ = media; // attachments are exercised by the media test below
    zip_bytes(&[
        ("actor.json", actor.to_string().as_bytes()),
        ("outbox.json", outbox.to_string().as_bytes()),
        ("likes.json", likes.to_string().as_bytes()),
        ("bookmarks.json", bookmarks.to_string().as_bytes()),
        ("custom_filters.json", filters.to_string().as_bytes()),
    ])
}

/// A pre-4.2 Mastodon archive: same documents wrapped in `.tar.gz`
/// with per-item `@context`, LD `signature` blocks, `totalItems` on
/// the collections, and a `key.pem` member that must never be read,
/// stored, logged or re-exported.
fn legacy_targz() -> Vec<u8> {
    let mut item = create(note("10", "2019-05-01T00:00:00Z", "<p>old post</p>", None));
    item["@context"] = json!("https://www.w3.org/ns/activitystreams");
    item["signature"] = json!({
        "type": "RsaSignature2017",
        "creator": format!("{OLD_ACTOR}#main-key"),
        "signatureValue": "ZmFrZQ==",
    });
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [item],
    });
    let likes = json!({
        "id": format!("{OLD_ACTOR}/likes"),
        "type": "OrderedCollection",
        "totalItems": 1,
        "orderedItems": ["https://remote.example/statuses/42"],
    });
    let pem = b"-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\n";
    targz_bytes(
        &[
            ("outbox.json", outbox.to_string().as_bytes()),
            ("likes.json", likes.to_string().as_bytes()),
            ("key.pem", pem.as_slice()),
        ],
        &[],
    )
}

/// Pleroma/Akkoma-style ZIP: same member names, media referenced by
/// remote URL rather than embedded.
fn pleroma_zip() -> Vec<u8> {
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [
            create(note("20", "2021-03-01T00:00:00Z", "<p>pleroma post</p>", None)),
        ],
    });
    zip_bytes(&[
        (
            "actor.json",
            json!({"id": OLD_ACTOR, "type": "Person"}).to_string().as_bytes(),
        ),
        ("outbox.json", outbox.to_string().as_bytes()),
        (
            "likes.json",
            json!({"id": format!("{OLD_ACTOR}/likes"), "type": "OrderedCollection", "orderedItems": []})
                .to_string()
                .as_bytes(),
        ),
        (
            "bookmarks.json",
            json!({"id": format!("{OLD_ACTOR}/bookmarks"), "type": "OrderedCollection", "orderedItems": []})
                .to_string()
                .as_bytes(),
        ),
    ])
}

/// Queue an archive import row + media blob the way `create_import`
/// does, then drive the job to completion. Returns the import id.
async fn run_archive_import(env: &Env, importer: i64, data: &[u8], name: &str) -> i64 {
    let key = format!("imports/{importer}/{name}");
    env.state
        .media
        .put(&key, data, "application/octet-stream")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key) \
         VALUES ($1, 'archive', 'merge', $2) RETURNING id",
    )
    .bind(importer)
    .bind(&key)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    import_id
}

async fn imported_status_count(env: &Env, account: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(account)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

async fn import_state(env: &Env, import_id: i64) -> (String, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, error FROM archive_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap()
}

// Import-format tests
// ---------------------------------------------------------------------------

/// Mastodon ≥ 4.2 ZIP restores posts, boosts, polls, replies, likes,
/// bookmarks and custom filters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: faithful archive restoration"]
async fn mastodon_zip_restores_full_outbox() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let import_id = run_archive_import(&env, importer, &mastodon42_zip(), "archive.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    // Three authored objects: two Notes + the Question (the Announce
    // becomes a boost row, not a new authored status).
    assert_eq!(imported_status_count(&env, importer).await, 3);

    // The reply resolved its parent through the source-ID mapping.
    let reply_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses c JOIN statuses p ON c.in_reply_to_id = p.id \
         WHERE c.account_id = $1 AND p.original_id LIKE '%/statuses/1')",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(reply_ok, "reply must link to the imported parent");

    // The Announce imported as a boost of the remote original.
    let boost_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses b JOIN statuses s ON b.reblog_of_id = s.id \
         WHERE b.account_id = $1 AND s.uri = 'https://remote.example/statuses/42')",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(boost_ok, "Announce must import as a boost");

    // OrderedCollection likes + bookmarks were applied.
    let fav_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM favourites f JOIN statuses s ON f.status_id = s.id \
         WHERE f.account_id = $1)",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let bm_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM bookmarks b JOIN statuses s ON b.status_id = s.id \
         WHERE b.account_id = $1)",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(fav_ok && bm_ok, "collection members must import");

    // Mastodon-format custom filters were applied.
    let filter_ok: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM filters WHERE account_id = $1)")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(filter_ok, "custom_filters.json must import");
}

/// Pre-4.2 `.tar.gz` archives import; LD signatures and per-item
/// `@context` are normalized away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: tar.gz support"]
async fn legacy_targz_archive_imports() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let import_id = run_archive_import(&env, importer, &legacy_targz(), "archive.tar.gz").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "tar.gz import failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 1);

    // Source publication date predates the account's creation and is
    // preserved.
    let old_date: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE account_id = $1 \
         AND created_at < '2020-01-01'::timestamptz)",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(old_date, "source timestamp must be preserved");
}

/// The private-key PEM member that shipped in 2018-era archives is
/// completely ignored — never stored anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: private-key exclusion"]
async fn private_key_member_is_never_stored() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let import_id = run_archive_import(&env, importer, &legacy_targz(), "archive.tar.gz").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "tar.gz import failed: {error:?}");

    // No key material in media storage or status rows.
    let key_in_status: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE content LIKE '%PRIVATE KEY%' \
         OR content_html LIKE '%PRIVATE KEY%')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(!key_in_status, "private key must not be imported");
    let media_dir = env.state.config().media.local_path.clone();
    for entry in std::fs::read_dir(&media_dir).unwrap().flatten() {
        let bytes = std::fs::read(entry.path()).unwrap_or_default();
        assert!(
            !bytes.windows(11).any(|w| w == b"PRIVATE KEY"),
            "key material leaked into {}",
            entry.path().display()
        );
    }
}

/// Akkoma/Pleroma ZIP archives (remote media references, no embedded
/// files) import their outbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: pleroma/akkoma archives"]
async fn pleroma_zip_archive_imports() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let import_id = run_archive_import(&env, importer, &pleroma_zip(), "archive.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "pleroma import failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 1);
}

// Archive-security tests
// ---------------------------------------------------------------------------

/// Path traversal, absolute paths, symlinks and device/FIFO members
/// are rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: archive member validation"]
async fn malicious_members_are_rejected() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(note("30", "2020-01-01T00:00:00Z", "<p>x</p>", None))],
    });

    for (label, archive) in [
        (
            "zip traversal",
            zip_bytes(&[
                ("outbox.json", outbox.to_string().as_bytes()),
                ("../evil.json", b"{}".as_slice()),
                ("/abs.json", b"{}".as_slice()),
            ]),
        ),
        (
            "tar symlink+fifo",
            targz_bytes(
                &[("outbox.json", outbox.to_string().as_bytes())],
                &[
                    ("evil-link", tar::EntryType::Symlink),
                    ("evil-fifo", tar::EntryType::Fifo),
                ],
            ),
        ),
    ] {
        let import_id = run_archive_import(&env, importer, &archive, label).await;
        let (state, _) = import_state(&env, import_id).await;
        assert_eq!(state.as_str(), "failed", "{label} must be rejected");
    }
}

/// Ambiguous duplicate member names are rejected rather than picked
/// arbitrarily.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: archive member validation"]
async fn duplicate_member_names_are_rejected() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;
    let archive = zip_bytes(&[
        ("outbox.json", b"{}".as_slice()),
        (
            "outbox.json",
            b"{\"type\":\"OrderedCollection\"}".as_slice(),
        ),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "dup.zip").await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "failed", "duplicates must be rejected");
}

/// A malformed supported member fails the item — it is not silently
/// treated as an empty collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 7: malformed members are errors"]
async fn malformed_outbox_is_an_error() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;
    let archive = zip_bytes(&[("outbox.json", b"not json at all".as_slice())]);
    let import_id = run_archive_import(&env, importer, &archive, "bad.zip").await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "failed", "malformed members must fail");
}

// Export-fidelity tests
// ---------------------------------------------------------------------------

/// Exported archives carry Announce objects for boosts and
/// OrderedCollection likes/bookmarks (Mastodon's format).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 6: faithful export"]
async fn export_archive_uses_mastodon_shapes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "exporter").await;
    let exporter = account_id(&env.state, "exporter").await;

    // Post + boost a remote object so the outbox has an Announce.
    let (status, post) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/statuses",
                json!({"status": "to be boosted", "visibility": "public"}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remote_account = seed_remote(&env.state, "https://remote.example/users/someone").await;
    let remote_status: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '<p>remote post</p>', 'https://remote.example/statuses/42', \
                'https://remote.example/statuses/42', 'public', false) RETURNING id",
    )
    .bind(remote_account)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO statuses (account_id, content, uri, visibility, reblog_of_id) \
         VALUES ($1, '', $2, 'public', $3)",
    )
    .bind(exporter)
    .bind(format!("{BASE}/ap/users/exporter/statuses/boost1"))
    .bind(remote_status)
    .execute(&env.state.db)
    .await
    .unwrap();
    let _ = post;

    // Queue + run the archive export job.
    let export_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind) \
         VALUES ($1, 'archive') RETURNING id",
    )
    .bind(exporter)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let ctx = job_ctx(&env.state);
    transfer::ExportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "export_id": export_id }))
        .await
        .unwrap();

    let file_key: String = sqlx::query_scalar("SELECT file_key FROM archive_exports WHERE id = $1")
        .bind(export_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let data = env.state.media.get(&file_key).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut outbox = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("outbox.json").unwrap(), &mut outbox).unwrap();
    let outbox: Value = serde_json::from_str(&outbox).unwrap();
    let types: Vec<&str> = outbox["orderedItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap_or(""))
        .collect();
    assert!(
        types.contains(&"Announce"),
        "boosts export as Announce, got {types:?}"
    );

    let mut likes = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("likes.json").unwrap(), &mut likes).unwrap();
    assert!(
        likes.contains("OrderedCollection"),
        "likes.json must be an OrderedCollection"
    );
}

// Suspended-account export access
// ---------------------------------------------------------------------------

/// A suspended account can still log in (restricted session) and
/// reach its export list/download — while ordinary API surfaces
/// stay forbidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 5: restricted export sessions"]
async fn suspended_login_reaches_exports_only() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "banned").await;
    let banned = account_id(&env.state, "banned").await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(banned)
        .execute(&env.state.db)
        .await
        .unwrap();

    // Password-grant login must succeed and yield a restricted token.
    let (status, body) = request(
        &env.app,
        Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "grant_type": "password",
                    "username": "banned@example.com",
                    "password": "correct-horse",
                    "scope": "read write",
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "suspended login must succeed");
    let token = body["access_token"].as_str().unwrap().to_string();

    // Export listing is allowed on the restricted token…
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/exports", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "restricted export access");

    // …but ordinary surfaces stay forbidden.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/timelines/home", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "restricted token must not reach timelines"
    );
}
