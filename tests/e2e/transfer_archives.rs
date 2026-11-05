//! Account-archive restoration tests: real Mastodon (ZIP + pre-4.2
//! tar.gz), Akkoma and Pleroma archive shapes built in memory, driven
//! through the import job — plus archive-security rejection cases and
//! export-fidelity checks.
//!
//! Exercises archive and API imports, publication, historical overlays,
//! and restricted-export sessions.
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
use redis::AsyncCommands;
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

/// A real 4×4 PNG — embedded media now goes through the upload
/// pipeline's bounded decode, so signature-only bytes don't qualify.
fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// Minimal multipart body: `file` + text fields (same shape as the
/// other e2e suites' helper).
fn multipart(file_name: &str, file: &[u8], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "yondertestboundary";
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), out)
}

fn upload_req(path: &str, content_type: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap()
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
    run_archive_import_opts(env, importer, data, name, json!({})).await
}

/// `run_archive_import` with immutable session `options`.
async fn run_archive_import_opts(
    env: &Env,
    importer: i64,
    data: &[u8],
    name: &str,
    options: Value,
) -> i64 {
    let key = format!("imports/{importer}/{name}");
    env.state
        .media
        .put(&key, data, "application/octet-stream")
        .await
        .unwrap();
    let import_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key, options) \
         VALUES ($1, 'archive', 'merge', $2, $3) RETURNING id",
    )
    .bind(importer)
    .bind(&key)
    .bind(options)
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
    sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND reblog_of_id IS NULL",
    )
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
async fn mastodon_zip_restores_full_outbox() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    // The Announce's target must be materialized for the boost to
    // write synchronously — unmaterialized targets park in
    // `pending_boosts` instead of wrapping an empty stub.
    let remote_account = seed_remote(&env.state, "https://remote.example/users/other").await;
    sqlx::query(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '<p>remote</p>', 'https://remote.example/statuses/42', \
                'https://remote.example/statuses/42', 'public', false)",
    )
    .bind(remote_account)
    .execute(&env.state.db)
    .await
    .unwrap();

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

    // Poll tallies carry as historical votes (a=3, b=5 in the
    // fixture), and source timestamps older than the account survive.
    let tallies: Vec<i32> = sqlx::query_scalar(
        "SELECT po.votes_count FROM poll_options po \
         JOIN polls p ON p.id = po.poll_id \
         JOIN statuses s ON s.poll_id = p.id \
         WHERE s.account_id = $1 ORDER BY po.idx",
    )
    .bind(importer)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(tallies, vec![3, 5], "poll tallies must import");
    let old_ts_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE s.account_id = $1 AND s.original_created_at < a.created_at)",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(old_ts_ok, "source dates before signup must be preserved");
}

/// Pre-4.2 `.tar.gz` archives import; LD signatures and per-item
/// `@context` are normalized away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
async fn duplicate_member_names_are_rejected() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;
    // `zip` refuses duplicate names at write time, so exercise the
    // ambiguous-name rejection through the tar.gz path (tar allows
    // repeated member names in the format itself).
    let archive = targz_bytes(
        &[
            ("outbox.json", b"{}".as_slice()),
            (
                "outbox.json",
                b"{\"type\":\"OrderedCollection\"}".as_slice(),
            ),
        ],
        &[],
    );
    let import_id = run_archive_import(&env, importer, &archive, "dup.zip").await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "failed", "duplicates must be rejected");
}

/// A malformed supported member fails the item — it is not silently
/// treated as an empty collection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_outbox_is_an_error() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;
    for member in [b"not json at all".as_slice(), b"{}".as_slice()] {
        let archive = zip_bytes(&[("outbox.json", member)]);
        let import_id = run_archive_import(&env, importer, &archive, "bad.zip").await;
        let (state, _) = import_state(&env, import_id).await;
        assert_eq!(state.as_str(), "failed", "malformed members must fail");
    }
    // No recognized member at all is a failure too — a `done` row
    // with zero items would be a silent no-op the user can't see.
    let archive = zip_bytes(&[("readme.txt", b"hello".as_slice())]);
    let import_id = run_archive_import(&env, importer, &archive, "none.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "failed", "empty-member archives must fail");
    assert!(
        error.unwrap_or_default().contains("no importable members"),
        "expected an explicit error"
    );
}

/// Archives wrapped in a single top-level directory (e.g. a foldered
/// export) normalize to bare member names instead of importing zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapped_archive_imports() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let n = note("90", "2021-01-01T00:00:00Z", "<p>wrapped</p>", None);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[
        ("export/actor.json", b"{}".as_slice()),
        ("export/outbox.json", outbox.to_string().as_bytes()),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "wrapped.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "wrapped import failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 1);
}

// Export-fidelity tests
// ---------------------------------------------------------------------------

/// Exported archives carry Announce objects for boosts and
/// OrderedCollection likes/bookmarks (Mastodon's format).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

/// Replies whose parent appears later in the outbox resolve through
/// the end-of-import graph fix-up — archive order must not matter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_reply_resolves_via_fixup() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    // Reply first, parent second.
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [
            create(note("2", "2020-01-04T00:00:00Z", "<p>reply</p>", Some("1"))),
            create(note("1", "2020-01-02T00:00:00Z", "<p>parent</p>", None)),
        ],
    });
    let archive = zip_bytes(&[("outbox.json", outbox.to_string().as_bytes())]);
    let import_id = run_archive_import(&env, importer, &archive, "ooo.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");
    let linked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses c JOIN statuses p ON c.in_reply_to_id = p.id \
         WHERE c.account_id = $1 AND p.original_id LIKE '%/statuses/1')",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(linked, "out-of-order reply must link via fix-up");
}

/// Attachment members ship inside the archive: their bytes land in
/// media storage and the status links the stored copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_media_bytes_are_restored() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut n = note("9", "2020-01-02T00:00:00Z", "<p>with pic</p>", None);
    n["attachment"] = json!([{
        "type": "Document",
        "mediaType": "image/png",
        "url": "media_attachments/att9.png",
        "name": "an image",
    }]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[
        ("outbox.json", outbox.to_string().as_bytes()),
        ("media_attachments/att9.png", png().as_slice()),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "media.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let key: Option<String> = sqlx::query_scalar(
        "SELECT file_path FROM media_attachments m JOIN statuses s ON s.id = m.status_id \
         WHERE s.account_id = $1",
    )
    .bind(importer)
    .fetch_optional(&env.state.db)
    .await
    .unwrap();
    let key = key.expect("attachment row must exist");
    let stored =
        std::fs::read(std::path::Path::new(&env.state.config().media.local_path).join(&key))
            .expect("media bytes must be stored");
    assert!(
        stored.starts_with(b"\x89PNG"),
        "stored bytes must be a real png (decoded + re-encoded)"
    );
}

/// A `direct`-audience Create never becomes a status — it is stored
/// as an owner-only private history message (contract §7.5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_messages_become_private_history() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut n = note("20", "2020-06-01T00:00:00Z", "<p>secret</p>", None);
    n["to"] = json!(["https://remote.example/users/alice"]);
    n["tag"] = json!([{
        "type": "Mention",
        "href": "https://remote.example/users/alice",
    }]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[("outbox.json", outbox.to_string().as_bytes())]);
    let import_id = run_archive_import(&env, importer, &archive, "dm.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let dm_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM private_history_messages \
         WHERE owner_id = $1 AND source_uri LIKE '%/statuses/20' \
           AND sender_uri = $2)",
    )
    .bind(importer)
    .bind(OLD_ACTOR)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(dm_ok, "DM must land in private_history_messages");
    assert_eq!(
        imported_status_count(&env, importer).await,
        0,
        "a DM must not become a status"
    );
}

/// `./`-prefixed member names (the `tar -C dir .` layout) normalize
/// instead of being rejected; zip symlink members are rejected; an
/// oversized member — a deflate bomb — is rejected before it can
/// inflate fully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_member_security_cases() {
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

    // `./` prefixes normalize (zip path; the tar builder normalizes
    // them away itself, so zip carries the check end-to-end).
    let ok = zip_bytes(&[("./outbox.json", outbox.to_string().as_bytes())]);
    let import_id = run_archive_import(&env, importer, &ok, "dot.zip").await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "dot-prefixed members must import");

    // Zip symlink entries are rejected too — covered at the unit
    // level (the zip crate can't write unix file-type bits, so the
    // fixture is hand-rolled in archive.rs tests); tar symlink/FIFO
    // rejection is covered by `malicious_members_are_rejected`.

    // >64 MiB of zeros deflates to a few KB — a classic bomb shape.
    let big = vec![0u8; 65 * 1024 * 1024];
    let bomb = zip_bytes(&[("big.bin", big.as_slice())]);
    let import_id = run_archive_import(&env, importer, &bomb, "bomb.zip").await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(
        state.as_str(),
        "failed",
        "oversized member must be rejected"
    );
}

/// The restricted surface has sharp edges in both directions: an
/// ordinary token dies at suspension, the restricted token reaches
/// only its session metadata + exports, and logout revokes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_session_boundaries() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let normal_token = register(&env, "normal").await;
    let banned_token = register(&env, "banned").await;
    let banned = account_id(&env.state, "banned").await;

    // Ordinary caller is refused the restricted surface.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/restricted/session", json!(null)),
            &normal_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(banned)
        .execute(&env.state.db)
        .await
        .unwrap();

    // The pre-suspension token no longer resolves at all.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/timelines/home", json!(null)),
            &banned_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Fresh password login yields the restricted token.
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
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["yonder"]["restricted"], json!(true));
    let restricted = body["access_token"].as_str().unwrap().to_string();

    // Restricted session metadata: own email + contact, nothing else.
    let (status, session) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/restricted/session", json!(null)),
            &restricted,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["restricted"], json!(true));
    assert_eq!(session["email"], json!("banned@example.com"));

    // Imports and ordinary surfaces stay forbidden — writes *and*
    // reads (import status is outside the restricted surface).
    for (method, uri) in [
        ("POST", "/api/v1/yonder/imports"),
        ("GET", "/api/v1/yonder/imports"),
    ] {
        let (status, _) = request(
            &env.app,
            authed(json_req(method, uri, json!(null)), &restricted),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // Logout revokes the credential entirely.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/yonder/restricted/logout", json!(null)),
            &restricted,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/exports", json!(null)),
            &restricted,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Unsuspension kills a live restricted token — a fresh normal
    // login is required.
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
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let restricted2 = body["access_token"].as_str().unwrap().to_string();
    sqlx::query("UPDATE accounts SET suspended_at = NULL WHERE id = $1")
        .bind(banned)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/yonder/exports", json!(null)),
            &restricted2,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Disabled accounts never log in, restricted or otherwise.
    sqlx::query("UPDATE accounts SET disabled_at = now() WHERE id = $1")
        .bind(banned)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _) = request(
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
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Case-varied `.pem` names, renamed PEM content, and armored keys
/// hidden behind a preamble are all dropped at indexing — key
/// material never reaches storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renamed_key_material_is_never_stored() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let pem = b"-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n";
    let archive = targz_bytes(
        &[
            (
                "outbox.json",
                br#"{"type":"OrderedCollection","orderedItems":[]}"#.as_slice(),
            ),
            ("KEY.PEM", pem.as_slice()),
            ("notes.txt", pem.as_slice()),
            (
                "bag.bin",
                b"Bag Attributes\n    x\n-----BEGIN PRIVATE KEY-----\nMIIE".as_slice(),
            ),
        ],
        &[],
    );
    let import_id = run_archive_import(&env, importer, &archive, "archive.tar.gz").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "tar.gz import failed: {error:?}");

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

/// Re-running the same archive is a no-op: statuses dedupe via the
/// pre-check, stored media bytes are untouched, the run still reports
/// `done` and counts the item as already present.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaying_an_import_is_idempotent() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut n = note("77", "2020-06-01T00:00:00Z", "<p>replay</p>", None);
    n["attachment"] = json!([{
        "type": "Document",
        "mediaType": "image/png",
        "url": "media_attachments/pic.png",
    }]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[
        ("outbox.json", outbox.to_string().as_bytes()),
        ("media_attachments/pic.png", png().as_slice()),
    ]);

    let import_id = run_archive_import(&env, importer, &archive, "a.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let key: String = sqlx::query_scalar(
        "SELECT file_path FROM media_attachments m JOIN statuses s ON s.id = m.status_id \
         WHERE s.account_id = $1",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let before =
        std::fs::read(std::path::Path::new(&env.state.config().media.local_path).join(&key))
            .expect("media bytes stored");

    let import_id = run_archive_import(&env, importer, &archive, "a.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "replay failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 1);
    let after =
        std::fs::read(std::path::Path::new(&env.state.config().media.local_path).join(&key))
            .expect("media bytes still stored");
    assert_eq!(before, after, "replay must not rewrite stored media");
    let present: i64 = sqlx::query_scalar(
        "SELECT COALESCE((summary->>'already_present')::bigint, 0) \
         FROM archive_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(present, 1, "replay must count the dedupe hit");
}

/// Two attachments with identical bytes share one content-addressed
/// storage key after import — export must still write a member for
/// every outbox reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_media_exports_every_reference() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut a = note("80", "2020-01-01T00:00:00Z", "<p>one</p>", None);
    a["attachment"] = json!([{
        "type": "Document", "mediaType": "image/png",
        "url": "media_attachments/a.png",
    }]);
    let mut b = note("81", "2020-01-02T00:00:00Z", "<p>two</p>", None);
    b["attachment"] = json!([{
        "type": "Document", "mediaType": "image/png",
        "url": "media_attachments/b.png",
    }]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(a), create(b)],
    });
    let bytes = png();
    let archive = zip_bytes(&[
        ("outbox.json", outbox.to_string().as_bytes()),
        ("media_attachments/a.png", bytes.as_slice()),
        ("media_attachments/b.png", bytes.as_slice()),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "dup.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT file_path FROM media_attachments WHERE account_id = $1 ORDER BY id",
    )
    .bind(importer)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(paths.len(), 2, "both attachment rows must exist");
    assert_eq!(paths[0], paths[1], "identical bytes share one key");

    let export_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind) \
         VALUES ($1, 'archive') RETURNING id",
    )
    .bind(importer)
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
    let names: std::collections::HashSet<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    let mut outbox = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("outbox.json").unwrap(), &mut outbox).unwrap();
    let outbox: Value = serde_json::from_str(&outbox).unwrap();
    let urls: Vec<&str> = outbox["orderedItems"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|i| {
            i["object"]["attachment"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d["url"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(urls.len(), 2, "both attachments must be exported");
    for url in urls {
        assert!(
            names.contains(url),
            "outbox references missing member {url}"
        );
    }
}

/// A media row whose storage object is gone is listed in
/// `missing_media.txt` instead of silently producing a dangling
/// outbox reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_media_objects_are_reported() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "exporter").await;
    let exporter = account_id(&env.state, "exporter").await;

    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri, visibility, local) \
         VALUES ($1, '<p>gone</p>', 'http://127.0.0.1:4000/ap/users/exporter/statuses/9', \
                 'public', true) RETURNING id",
    )
    .bind(exporter)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_attachments (account_id, status_id, type, file_path) \
         VALUES ($1, $2, 'image', $3)",
    )
    .bind(exporter)
    .bind(status_id)
    .bind("imports/9/definitely-missing.png")
    .execute(&env.state.db)
    .await
    .unwrap();
    // Profile images report through the same list.
    sqlx::query(
        "UPDATE accounts SET avatar_path = 'avatars/9/gone.png', \
         banner_path = 'headers/9/gone.png' WHERE id = $1",
    )
    .bind(exporter)
    .execute(&env.state.db)
    .await
    .unwrap();

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
    let mut report = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("missing_media.txt").unwrap(), &mut report)
        .unwrap();
    for key in [
        "imports/9/definitely-missing.png",
        "avatars/9/gone.png",
        "headers/9/gone.png",
    ] {
        assert!(
            report.contains(key),
            "missing {key} must be reported, got {report}"
        );
    }
}

/// A corrupt archive upload is refused at request time (422), not
/// left to fail asynchronously inside the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_archive_upload_is_rejected_upfront() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "uploader").await;

    // Zip magic followed by garbage has no central directory.
    let (ct, body) = multipart(
        "a.zip",
        b"PK\x03\x04definitely-not-a-zip",
        &[("kind", "archive")],
    );
    let (status, err) = request(
        &env.app,
        authed(upload_req("/api/v1/yonder/imports", &ct, body), &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

    // A real zip is accepted and queued.
    let ok = zip_bytes(&[("outbox.json", b"{\"orderedItems\":[]}".as_slice())]);
    let (ct, body) = multipart("a.zip", &ok, &[("kind", "archive")]);
    let (status, resp) = request(
        &env.app,
        authed(upload_req("/api/v1/yonder/imports", &ct, body), &token),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
}

/// The 202 body is projected from the committed row: its id resolves to
/// the same session on the status endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_response_projects_the_committed_row() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "uploadshape").await;
    let ok = zip_bytes(&[("outbox.json", b"{\"orderedItems\":[]}".as_slice())]);
    let (ct, body) = multipart("a.zip", &ok, &[("kind", "archive")]);
    let (status, resp) = request(
        &env.app,
        authed(upload_req("/api/v1/yonder/imports", &ct, body), &token),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    assert_eq!(resp["kind"], "archive");
    assert_eq!(resp["state"], "pending", "{resp}");
    let id = resp["id"]
        .as_str()
        .expect("id is a decimal string")
        .to_string();
    let get = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/yonder/imports/{id}"))
        .body(Body::empty())
        .unwrap();
    let (status, got) = request(&env.app, authed(get, &token)).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["id"], resp["id"]);
    assert_eq!(got["kind"], resp["kind"]);
}

/// Remote references keep only absolute `http(s)` URLs — a refused
/// embedded member's archive-relative path and arbitrary schemes are
/// dropped instead of becoming broken `remote_url` links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_reference_urls_are_validated() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut n = note("95", "2021-05-01T00:00:00Z", "<p>refs</p>", None);
    n["attachment"] = json!([
        // A real remote URL → kept as a reference.
        { "type": "Document", "mediaType": "image/png",
          "url": "https://cdn.example/x.png" },
        // A member that was refused by validation — the
        // archive-relative name must not become a remote URL.
        { "type": "Document", "mediaType": "text/html",
          "url": "media_attachments/x.html" },
        // An arbitrary scheme → dropped, never served to clients.
        { "type": "Document", "mediaType": "image/png",
          "url": "javascript:alert(1)" },
    ]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[
        ("outbox.json", outbox.to_string().as_bytes()),
        ("media_attachments/x.html", b"<html>x</html>".as_slice()),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "refs.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT file_path, remote_url FROM media_attachments \
         WHERE account_id = $1 ORDER BY id",
    )
    .bind(importer)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(None, Some("https://cdn.example/x.png".to_string()))],
        "only the absolute http(s) reference survives"
    );
    let unavailable: i64 = sqlx::query_scalar(
        "SELECT COALESCE((summary->>'media_unavailable')::bigint, 0) \
         FROM archive_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(unavailable, 2, "refused and unsafe attachments are counted");
}

/// A Create whose `actor` does not match the object's `attributedTo`
/// is not the importer's own work — it is counted unsupported, never
/// written under their identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_mismatch_is_unsupported() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut c = create(note("96", "2021-06-01T00:00:00Z", "<p>x</p>", None));
    c["object"]["attributedTo"] = json!("https://elsewhere.example/users/someone");
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [c],
    });
    let archive = zip_bytes(&[("outbox.json", outbox.to_string().as_bytes())]);
    let import_id = run_archive_import(&env, importer, &archive, "mismatch.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 0);
    let unsupported: i64 = sqlx::query_scalar(
        "SELECT COALESCE((summary->>'unsupported')::bigint, 0) \
         FROM archive_imports WHERE id = $1",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(unsupported, 1);
}

/// A real JPEG carrying an APP1 EXIF segment — imported images go
/// through the upload pipeline's re-encode, so the stored bytes must
/// not keep the metadata.
fn jpeg_with_exif() -> Vec<u8> {
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(4, 4, image::Rgb([0, 255, 0]))
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
    let jpeg = jpeg.into_inner();
    let exif = b"Exif\0\0FAKE-EXIF-PAYLOAD";
    let mut out = Vec::new();
    out.extend(&jpeg[..2]); // SOI
    out.extend([0xFF, 0xE1]);
    out.extend(((exif.len() + 2) as u16).to_be_bytes());
    out.extend(exif);
    out.extend(&jpeg[2..]);
    out
}

/// Embedded images are re-encoded through the same pipeline as
/// uploads — the stored file keeps no EXIF metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_images_are_re_encoded() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let mut n = note("97", "2021-07-01T00:00:00Z", "<p>jpeg</p>", None);
    n["attachment"] = json!([{
        "type": "Document", "mediaType": "image/jpeg",
        "url": "media_attachments/photo.jpg",
    }]);
    let outbox = json!({
        "id": format!("{OLD_ACTOR}/outbox"),
        "type": "OrderedCollection",
        "orderedItems": [create(n)],
    });
    let archive = zip_bytes(&[
        ("outbox.json", outbox.to_string().as_bytes()),
        ("media_attachments/photo.jpg", jpeg_with_exif().as_slice()),
    ]);
    let import_id = run_archive_import(&env, importer, &archive, "jpeg.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let (key, processed): (String, bool) = sqlx::query_as(
        "SELECT file_path, processed FROM media_attachments \
         WHERE account_id = $1",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(processed, "stored bytes were processed — say so");
    let bytes =
        std::fs::read(std::path::Path::new(&env.state.config().media.local_path).join(&key))
            .expect("media bytes stored");
    assert!(
        !bytes.windows(4).any(|w| w == b"Exif"),
        "EXIF metadata must not survive the re-encode"
    );
}

/// Bookmarks rows go through the same stubbing as the archive's
/// `bookmarks.json`: a valid URI lands, anything else is a counted
/// failure — never a silent success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bookmark_csv_rows_are_applied_or_failed() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "importer").await;
    let importer = account_id(&env.state, "importer").await;

    let csv = b"https://remote.example/users/a/statuses/1\njavascript:alert(1)\n";
    let archive = zip_bytes(&[("bookmarks.csv", csv.as_slice())]);
    let import_id = run_archive_import(&env, importer, &archive, "bookmarks.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let (processed, failed): (i32, i32) =
        sqlx::query_as("SELECT processed, failed FROM archive_imports WHERE id = $1")
            .bind(import_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!((processed, failed), (1, 1));
    let bookmarked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM bookmarks WHERE account_id = $1")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(bookmarked, 1);
}

/// An export whose account entered deletion before the build finished
/// is not committed, and no stored object outlives the refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_is_discarded_once_account_is_pending_deletion() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "leaver").await;
    let leaver = account_id(&env.state, "leaver").await;
    let export_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind) VALUES ($1, 'archive') RETURNING id",
    )
    .bind(leaver)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_deletions (account_id, scheduled_at, state) \
         VALUES ($1, now(), 'pending')",
    )
    .bind(leaver)
    .execute(&env.state.db)
    .await
    .unwrap();

    transfer::ExportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "export_id": export_id }))
        .await
        .unwrap();

    let (state, error): (String, Option<String>) =
        sqlx::query_as("SELECT state, error FROM archive_exports WHERE id = $1")
            .bind(export_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(state, "failed");
    assert!(error.unwrap_or_default().contains("no longer eligible"));
    let stored = env
        .state
        .media
        .list_prefix(&format!("exports/{leaver}/"))
        .await
        .unwrap();
    assert!(
        stored.is_empty(),
        "no archive object may outlive the refusal"
    );
}

// ---------------------------------------------------------------------------
// Import publication and remote-import sessions (contract §8–§10).
// ---------------------------------------------------------------------------

/// `federate: false` imports must produce zero ActivityPub output,
/// notifications, or fan-out.
/// `federate` unset on an archive import ⇒ no `publication_work`
/// rows, no `activities`, no `deliveries` — an archive restore stays
/// completely off the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_federation_off_emits_no_activities() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "quiet").await;
    let importer = account_id(&env.state, "quiet").await;
    // A remote follower exists — federate-off must still emit nothing.
    let remote = seed_remote(&env.state, "https://remote.example/users/fan").await;
    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(remote)
        .bind(importer)
        .execute(&env.state.db)
        .await
        .unwrap();

    let import_id = run_archive_import(&env, importer, &mastodon42_zip(), "quiet.zip").await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");
    assert_eq!(imported_status_count(&env, importer).await, 3);

    let work: i64 =
        sqlx::query_scalar("SELECT count(*) FROM publication_work WHERE import_id = $1")
            .bind(import_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(work, 0, "federate-off must write no publication work");
    let (acts, deliveries): (i64, i64) =
        sqlx::query_as("SELECT count(*), (SELECT count(*) FROM deliveries) FROM activities")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(acts, 0, "federate-off must produce no activities");
    assert_eq!(deliveries, 0, "federate-off must produce no deliveries");

    // Bookkeeping exists even without publication.
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM import_items WHERE import_id = $1")
        .bind(import_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(items >= 4, "import_items rows map sources → destinations");
}

/// `federate: true` ⇒ durable `publication_work` per public authored
/// item, then the drain publishes each activity exactly once — a
/// repeated drain adds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_federation_on_delivers_once() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "loud").await;
    let importer = account_id(&env.state, "loud").await;
    let remote = seed_remote(&env.state, "https://remote.example/users/fan").await;
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, inbox, shared_inbox, actor_data) \
         VALUES ($1, $2, $3, '', $4)",
    )
    .bind(remote)
    .bind(importer)
    .bind("https://remote.example/users/fan/inbox")
    .bind(json!({"id": "https://remote.example/users/fan",
                 "inbox": "https://remote.example/users/fan/inbox"}))
    .execute(&env.state.db)
    .await
    .unwrap();

    let import_id = run_archive_import_opts(
        &env,
        importer,
        &mastodon42_zip(),
        "loud.zip",
        json!({"federate": true}),
    )
    .await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    // The fixture: three public Creates; the Announce has no audience
    // (direct ⇒ never bulk re-sent). Work rows exist but nothing has
    // been published yet — the drain is a separate job.
    let work: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(work, 3, "one work row per public authored item");
    let acts: i64 = sqlx::query_scalar("SELECT count(*) FROM activities")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(acts, 0, "the import job itself publishes nothing");

    // Drain.
    let ctx = job_ctx(&env.state);
    transfer::ImportPublishJob::new(env.state.ap.clone().unwrap())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (delivered, pending): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'delivered'), \
                count(*) FILTER (WHERE state = 'pending') \
         FROM publication_work WHERE import_id = $1",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!((delivered, pending), (3, 0), "all work drained");
    let acts: i64 = sqlx::query_scalar("SELECT count(*) FROM activities")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(acts, 3, "one activity per authored item");
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM deliveries")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(deliveries, 3, "one delivery per activity per follower");

    // Activities carry the deterministic id + original `published`.
    let deterministic: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM activities \
         WHERE activity_id LIKE '%#imports/%/Create' AND object->>'published' LIKE '2020-%')",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        deterministic,
        "import activities are deterministic + backdated"
    );

    // A repeated drain (dup job / worker restart) adds nothing.
    transfer::ImportPublishJob::new(env.state.ap.clone().unwrap())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (acts2, deliveries2): (i64, i64) =
        sqlx::query_as("SELECT count(*), (SELECT count(*) FROM deliveries) FROM activities")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!((acts2, deliveries2), (3, 3), "publication is once-only");
}

// Section 9–10: remote provider stub
// ---------------------------------------------------------------------------

/// A Mastodon-compatible stub the `Provider` client talks to over
/// real HTTP (`allow_private_targets` lets the test use 127.0.0.1).
/// The `/fail` counters exercise the pause-on-401 path.
struct StubProvider {
    origin: String,
    /// When set, `verify_credentials` identifies a *different*
    /// account — the worker must pause rather than import it.
    flip_identity: Arc<std::sync::atomic::AtomicBool>,
    /// `verify_credentials` returns an error-shaped 200 once this
    /// many calls have succeeded — tests profile-write safety.
    verify_fail_after: Arc<std::sync::atomic::AtomicUsize>,
    /// Remaining statuses pages answered with a non-JSON 200.
    malformed_statuses: Arc<std::sync::atomic::AtomicUsize>,
    /// `verify_credentials` reports empty display name, bio and fields.
    empty_profile: Arc<std::sync::atomic::AtomicBool>,
    /// WebFinger call index (0-based) answered with HTTP 429.
    webfinger_429_at: Arc<std::sync::atomic::AtomicUsize>,
    /// Conversations include a second thread carrying the same message.
    duplicate_dm: Arc<std::sync::atomic::AtomicBool>,
    /// Run once on the next statuses request: supersede the session's
    /// lease, as a second worker claiming the same import would.
    takeover: Arc<tokio::sync::Mutex<Option<(sqlx::PgPool, i64)>>>,
    cancel_on_statuses: Arc<tokio::sync::Mutex<Option<(sqlx::PgPool, i64)>>>,
    profile_fields: Arc<tokio::sync::Mutex<Option<Value>>>,
    /// When set, the first statuses item replies to a parent outside
    /// the walked set (`in_reply_to_id` is not among the page ids), so
    /// the importer must resolve its URI via `/api/v1/statuses/{id}`.
    reply_parent: Arc<std::sync::atomic::AtomicBool>,
}

/// Start the stub; returns its origin (`http://127.0.0.1:{port}`).
async fn stub_provider() -> StubProvider {
    use axum::{
        extract::{Path, Query},
        response::IntoResponse,
        routing::{get, post},
        Json as AJson, Router as ARouter,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let origin = Arc::new(format!("http://127.0.0.1:{port}"));
    let flip_identity = Arc::new(AtomicBool::new(false));
    let verify_fail_after = Arc::new(AtomicUsize::new(usize::MAX));
    let verify_calls = Arc::new(AtomicUsize::new(0));
    let malformed_statuses = Arc::new(AtomicUsize::new(0));
    let empty_profile = Arc::new(AtomicBool::new(false));
    let webfinger_429_at = Arc::new(AtomicUsize::new(usize::MAX));
    let webfinger_calls = Arc::new(AtomicUsize::new(0));
    let duplicate_dm = Arc::new(AtomicBool::new(false));
    let takeover: Arc<tokio::sync::Mutex<Option<(sqlx::PgPool, i64)>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let cancel_on_statuses: Arc<tokio::sync::Mutex<Option<(sqlx::PgPool, i64)>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let profile_fields = Arc::new(tokio::sync::Mutex::new(None));
    let reply_parent = Arc::new(AtomicBool::new(false));

    let o = origin.clone();
    let flip = flip_identity.clone();
    let fail_after = verify_fail_after.clone();
    let calls = verify_calls.clone();
    let empty = empty_profile.clone();
    let fields = profile_fields.clone();
    let verify = move || {
        let o = o.clone();
        let flip = flip.clone();
        let fail_after = fail_after.clone();
        let calls = calls.clone();
        let empty = empty.clone();
        let fields = fields.clone();
        async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n >= fail_after.load(Ordering::SeqCst) {
                return AJson(json!({"error": "temporarily unavailable"}));
            }
            if flip.load(Ordering::SeqCst) {
                return AJson(json!({
                    "id": "intr9", "acct": "intruder",
                    "url": format!("{o}/@intruder"),
                }));
            }
            if empty.load(Ordering::SeqCst) {
                return AJson(json!({
                    "id": "src1", "acct": "olduser", "url": format!("{o}/@olduser"),
                    "display_name": "", "note": "", "fields": [],
                }));
            }
            AJson(json!({
                "id": "src1",
                "acct": "olduser",
                "url": format!("{o}/@olduser"),
                "display_name": "Old User",
                "note": "<p>hi</p>",
                "fields": fields.lock().await.clone().unwrap_or_else(|| json!([
                    {"name": "blog", "value": "https://blog.example"}
                ])),
            }))
        }
    };
    let o = origin.clone();
    let wf_calls = webfinger_calls.clone();
    let wf_429 = webfinger_429_at.clone();
    let webfinger = move |Query(q): Query<HashMap<String, String>>| {
        let o = o.clone();
        let wf_calls = wf_calls.clone();
        let wf_429 = wf_429.clone();
        async move {
            if wf_calls.fetch_add(1, Ordering::SeqCst) == wf_429.load(Ordering::SeqCst) {
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    AJson(json!({"error": "slow down"})),
                )
                    .into_response();
            }
            // Resolve `acct:user@host` to a canonical actor URI —
            // remote.example actors map to that origin, everything
            // else resolves to the stub's own user.
            let resource = q.get("resource").cloned().unwrap_or_default();
            let href = resource
                .strip_prefix("acct:")
                .and_then(|a| a.split_once('@'))
                .map(|(name, host)| {
                    if host.starts_with("127.0.0.1") {
                        format!("{o}/users/{name}")
                    } else {
                        format!("https://{host}/users/{name}")
                    }
                })
                .unwrap_or_else(|| format!("{o}/users/olduser"));
            AJson(json!({
                "subject": resource,
                "links": [{"rel": "self", "type": "application/activity+json",
                           "href": href}],
            }))
            .into_response()
        }
    };
    let o = origin.clone();
    let malformed = malformed_statuses.clone();
    let takeover_slot = takeover.clone();
    let cancel_slot = cancel_on_statuses.clone();
    let rp = reply_parent.clone();
    let statuses = move |Path(_id): Path<String>| {
        let o = o.clone();
        let malformed = malformed.clone();
        let takeover_slot = takeover_slot.clone();
        let cancel_slot = cancel_slot.clone();
        let rp = rp.clone();
        async move {
            if let Some((pool, import_id)) = cancel_slot.lock().await.take() {
                sqlx::query("UPDATE archive_imports SET cancel_requested = true WHERE id = $1")
                    .bind(import_id)
                    .execute(&pool)
                    .await
                    .unwrap();
                return (StatusCode::UNAUTHORIZED, AJson(json!({"error": "revoked"})))
                    .into_response();
            }
            if let Some((pool, import_id)) = takeover_slot.lock().await.take() {
                sqlx::query("UPDATE archive_imports SET run_token = 'takeover' WHERE id = $1")
                    .bind(import_id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            if malformed.load(Ordering::SeqCst) > 0 {
                malformed.fetch_sub(1, Ordering::SeqCst);
                return axum::response::Html("<html>proxy error page</html>").into_response();
            }
            // One public Note + one self-boost; numeric ids exercise
            // the idmap reply linkage. `reply_parent` turns the Note
            // into a reply whose parent sits outside the walked set.
            let mut first = json!({
                "id": "101",
                "uri": format!("{o}/users/olduser/statuses/101"),
                "created_at": "2020-06-01T10:00:00Z",
                "content": "<p>first</p>", "visibility": "public",
                "account": {"id": "src1", "acct": "olduser",
                            "url": format!("{o}/@olduser")},
                "favourites_count": 7, "reblogs_count": 3,
                "media_attachments": [], "mentions": [],
                "tags": [{"name": "regress-tag"}],
            });
            if rp.load(Ordering::SeqCst) {
                first["in_reply_to_id"] = json!("9");
            }
            let second = json!({
                "id": "102",
                    "uri": format!("{o}/users/olduser/statuses/102"),
                    "created_at": "2020-06-02T10:00:00Z",
                    "content": "", "visibility": "public",
                    "account": {"id": "src1", "acct": "olduser",
                                "url": format!("{o}/@olduser")},
                    "reblog": {
                        "id": "r9",
                        "uri": "https://remote.example/statuses/9",
                        "created_at": "2020-06-02T10:00:00Z",
                        "account": {"id": "x", "acct": "other@remote.example",
                                    "url": "https://remote.example/@other"},
                    },
            });
            AJson(json!([first, second])).into_response()
        }
    };

    let app = ARouter::new()
        .route(
            "/api/v1/apps",
            post(|| async { AJson(json!({"client_id": "cid", "client_secret": "sec"})) }),
        )
        .route(
            "/oauth/token",
            post(|| async {
                AJson(json!({"access_token": "tok", "token_type": "Bearer", "scope": "read"}))
            }),
        )
        .route("/api/v1/accounts/verify_credentials", get(verify))
        .route("/.well-known/webfinger", get(webfinger))
        .route("/api/v1/accounts/{id}/statuses", get(statuses))
        // Single-status doc: the reply-parent lookup resolves a source
        // `in_reply_to_id` outside the walked set to its AP `uri`.
        .route(
            "/api/v1/statuses/{id}",
            get({
                let o = origin.clone();
                move |Path(id): Path<String>| {
                    let o = o.clone();
                    async move {
                        AJson(json!({
                            "id": id,
                            "uri": format!("{o}/statuses/{id}"),
                            "created_at": "2020-05-01T10:00:00Z",
                            "content": "<p>parent</p>", "visibility": "public",
                            "account": {"id": "src2", "acct": "other",
                                        "url": format!("{o}/@other")},
                        }))
                    }
                }
            }),
        )
        .route(
            "/api/v1/accounts/{id}/following",
            get(|| async { AJson(json!([])) }),
        )
        .route(
            "/api/v1/accounts/{id}/followers",
            get(|| async { AJson(json!([])) }),
        )
        .route("/api/v1/blocks", get(|| async { AJson(json!([])) }))
        .route("/api/v1/mutes", get(|| async { AJson(json!([])) }))
        // Optional capability: 404 ⇒ the session marks it unsupported.
        .route(
            "/api/v1/bookmarks",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    AJson(json!({"error": "gone"})),
                )
            }),
        )
        .route("/api/v1/favourites", get(|| async { AJson(json!([])) }))
        .route("/api/v1/conversations", {
            let dup = duplicate_dm.clone();
            get(move || {
                let dup = dup.clone();
                async move {
                    let last = json!({
                        "id": "m1", "uri": "https://remote.example/statuses/m1",
                        "created_at": "2020-06-03T09:00:00Z",
                        "content": "<p>secret</p>",
                        "account": {"id": "x", "acct": "friend@remote.example"},
                    });
                    let mut threads = vec![json!({
                        "id": "c1", "unread": false,
                        "accounts": [{"id": "x", "acct": "friend@remote.example",
                                      "url": "https://remote.example/@friend"}],
                        "last_status": last.clone(),
                    })];
                    // The same message surfaces again in a group thread
                    // whose other participant is someone else.
                    if dup.load(Ordering::SeqCst) {
                        threads.push(json!({
                            "id": "c2", "unread": false,
                            "accounts": [{"id": "y", "acct": "other@remote.example",
                                          "url": "https://remote.example/@other"}],
                            "last_status": last,
                        }));
                    }
                    AJson(json!(threads))
                }
            })
        })
        .route(
            "/api/v1/statuses/{id}/favourited_by",
            get(|| async {
                AJson(json!([
                    {"id": "f1", "acct": "fan@remote.example",
                     "url": "https://remote.example/@fan"},
                ]))
            }),
        )
        .route(
            "/api/v1/statuses/{id}/reblogged_by",
            get(|| async {
                AJson(json!([
                    {"id": "b1", "acct": "booster@remote.example",
                     "url": "https://remote.example/@booster"},
                ]))
            }),
        )
        .route(
            "/api/v1/statuses/{id}/context",
            get(|| async {
                AJson(json!({
                    "ancestors": [],
                    "descendants": [
                        {"id": "d1", "uri": "https://remote.example/statuses/d1",
                         "account": {"id": "a1", "acct": "replier@remote.example",
                                     "url": "https://remote.example/@replier"}},
                    ],
                }))
            }),
        )
        .route(
            "/api/v1/statuses/{id}/quotes",
            get(|| async {
                AJson(json!([
                    {"id": "q1", "uri": "https://remote.example/statuses/q1",
                     "account": {"id": "a2", "acct": "quoter@remote.example",
                                 "url": "https://remote.example/@quoter"}},
                ]))
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    StubProvider {
        origin: origin.as_ref().clone(),
        flip_identity,
        verify_fail_after,
        malformed_statuses,
        empty_profile,
        webfinger_429_at,
        duplicate_dm,
        takeover,
        cancel_on_statuses,
        profile_fields,
        reply_parent,
    }
}

/// Begin an authorization flow as the browser would: returns the
/// state token from the remote URL and the binding cookie the
/// response set.
async fn begin_flow(env: &Env, token: &str, origin: &str) -> (String, String) {
    begin_flow_to(env, token, origin, None).await
}

async fn begin_flow_to(
    env: &Env,
    token: &str,
    origin: &str,
    return_to: Option<&str>,
) -> (String, String) {
    let mut body = json!({"origin": origin, "categories": ["statuses"]});
    if let Some(path) = return_to {
        body["return_to"] = json!(path);
    }
    let res = env
        .app
        .clone()
        .oneshot(authed(
            json_req("POST", "/api/v1/yonder/import_connections", body),
            token,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "begin");
    let cookie = res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let auth_url = body["authorization_url"].as_str().unwrap().to_string();
    assert!(auth_url.starts_with(&format!("{origin}/oauth/authorize?")));
    let state_tok = url::Url::parse(&auth_url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    (state_tok, cookie)
}

/// Complete the OAuth dance against the stub: begin → simulate the
/// provider redirect in the same browser → callback → connection row.
async fn connect(env: &Env, token: &str, origin: &str) -> i64 {
    let (state_tok, cookie) = begin_flow(env, token, origin).await;
    let (status, _) = request(
        &env.app,
        Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/yonder/import_connections/callback?code=abc&state={state_tok}"
            ))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "callback redirects");

    sqlx::query_scalar("SELECT id FROM oauth_connections WHERE origin = $1")
        .bind(origin)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

/// A consent link started by one account but completed in another
/// browser (no binding cookie) must not create a connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_from_another_browser_is_refused() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "bindvictim").await;
    let stub = stub_provider().await;
    let (state_tok, _cookie) = begin_flow(&env, &token, &stub.origin).await;

    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/yonder/import_connections/callback?code=abc&state={state_tok}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let location = res.headers()["location"].to_str().unwrap().to_string();
    assert!(location.contains("import_error="), "refused: {location}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_connections WHERE origin = $1")
        .bind(&stub.origin)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "no connection bound without the browser cookie");
}

/// blocks/mutes walk the account-level Mastodon lists — a session on
/// them completes instead of finding a missing endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relationship_categories_use_mastodon_lists() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "relations").await;
    let _importer = account_id(&env.state, "relations").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({"connection_id": conn_id, "categories": ["blocks", "mutes"]}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let cats: Value =
        sqlx::query_scalar("SELECT category_state FROM archive_imports WHERE id = $1")
            .bind(import_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    for cat in ["blocks", "mutes"] {
        assert_eq!(cats[cat]["done"], json!(true), "{cat} finished: {cats}");
        assert!(
            cats[cat].get("unsupported").is_none(),
            "{cat} endpoint exists: {cats}"
        );
    }
}

/// A full remote import against the stub: verified OAuth → session →
/// statuses/boosts/DMs → observations, owner-scoped reads, and the
/// `yonder.imported` provenance marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_oauth_import_end_to_end() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    // Remote credentials are AEAD-encrypted — the session fails
    // closed without a secret key, so the test sets one.
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "remoteuser").await;
    let importer = account_id(&env.state, "remoteuser").await;
    let stub = stub_provider().await;

    let conn_id = connect(&env, &token, &stub.origin).await;
    // The verified identity bound to the connection.
    let (source_uri, acct): (String, String) =
        sqlx::query_as("SELECT source_actor_uri, source_acct FROM oauth_connections WHERE id = $1")
            .bind(conn_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(source_uri.ends_with("/users/olduser"));
    assert!(acct.starts_with("olduser@"));

    // Credentials never readable in plaintext.
    let enc: Vec<u8> =
        sqlx::query_scalar("SELECT access_token_enc FROM oauth_connections WHERE id = $1")
            .bind(conn_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(!enc.windows(3).any(|w| w == b"tok"), "token is encrypted");

    // The outbox's reblog target must be materialized for the boost
    // to write synchronously — unmaterialized targets park in
    // `pending_boosts` instead of wrapping an empty stub.
    let remote_account = seed_remote(&env.state, "https://remote.example/users/other").await;
    sqlx::query(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '<p>remote</p>', 'https://remote.example/statuses/9', \
                'https://remote.example/statuses/9', 'public', false)",
    )
    .bind(remote_account)
    .execute(&env.state.db)
    .await
    .unwrap();

    // Session: statuses + dms + observations + profile.
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({
                    "connection_id": conn_id,
                    "categories": ["statuses", "dms", "observations", "profile"],
                }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    let ctx = job_ctx(&env.state);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    // One Note imported (the boost row is a reblog), DM history row,
    // and observations attached to the imported copy.
    let note_id: i64 = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE account_id = $1 AND original_id LIKE '%/statuses/101'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (likes, shares): (i32, i32) = sqlx::query_as(
        "SELECT remote_likes_count, remote_shares_count FROM statuses WHERE id = $1",
    )
    .bind(note_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!((likes, shares), (7, 3), "remote counters stay provenance");

    let boost: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses b JOIN statuses s ON b.reblog_of_id = s.id \
         WHERE b.account_id = $1 AND s.uri = 'https://remote.example/statuses/9')",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(boost, "the self-boost imported");

    let dm: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM private_history_messages \
         WHERE owner_id = $1 AND source_uri = 'https://remote.example/statuses/m1')",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(dm, "received DM lands in owner-only history");

    // Observations: like, boost, reply, quote — all owner-scoped,
    // none fabricated as live interactions.
    let obs: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM historical_observations WHERE owner_id = $1 AND status_id = $2 \
         ORDER BY kind",
    )
    .bind(importer)
    .bind(note_id)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(obs, vec!["boost", "like", "quote", "reply"]);

    // Live counters untouched by the overlays.
    let (fav, reb): (i32, i32) =
        sqlx::query_as("SELECT favourites_count, reblogs_count FROM statuses WHERE id = $1")
            .bind(note_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!((fav, reb), (0, 0), "no fabricated live interactions");

    // `yonder.imported` marks the copy; the history endpoint is
    // owner-scoped.
    let (status, body) = request(
        &env.app,
        authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/yonder/statuses/{note_id}/history"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "history: {body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert!(items.iter().any(|i| i["kind"] == "like"
        && i["actor_uri"]
            .as_str()
            .unwrap()
            .contains("remote.example/users/fan")));

    // Another account can't read the overlay.
    let other = register(&env, "outsider").await;
    let (status, _) = request(
        &env.app,
        authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/yonder/statuses/{note_id}/history"))
                .body(Body::empty())
                .unwrap(),
            &other,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "observations are owner-scoped"
    );

    // The status entity carries `yonder.imported`.
    let (status, body) = request(
        &env.app,
        authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/statuses/{note_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "status: {body}");
    assert!(body["yonder"]["imported"]["source_uri"]
        .as_str()
        .unwrap()
        .ends_with("/statuses/101"));
}

/// 401 from the provider pauses the session — `resume` re-enqueues
/// with a rebound connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_import_pauses_on_401() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "reauth").await;
    let _importer = account_id(&env.state, "reauth").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({"connection_id": conn_id, "categories": ["statuses"]}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // Revoke the remote side before the job runs — the driver
    // pauses on a dead connection rather than burning retries.
    sqlx::query("UPDATE oauth_connections SET state = 'revoked' WHERE id = $1")
        .bind(conn_id)
        .execute(&env.state.db)
        .await
        .unwrap();

    let ctx = job_ctx(&env.state);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&ctx, &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(
        state.as_str(),
        "paused",
        "dead connection pauses: {error:?}"
    );
    let error = error.unwrap();
    assert!(
        error.contains("reauthorize") || error.contains("connection"),
        "pause reason: {error}"
    );

    // Resuming with a usable connection un-pauses and re-enqueues.
    let conn2 = connect(&env, &token, &stub.origin).await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                &format!("/api/v1/yonder/imports/{import_id}/resume"),
                json!({"connection_id": conn2}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "resume accepted");
}

/// If the connected account changes after the session froze its
/// source, the worker pauses instead of importing a different
/// account — the fresh `verify_credentials` identity is re-resolved
/// and compared every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_import_pauses_on_identity_change() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "identitycheck").await;
    let _importer = account_id(&env.state, "identitycheck").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({"connection_id": conn_id, "categories": ["statuses"]}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // The provider now identifies a different account.
    stub.flip_identity
        .store(true, std::sync::atomic::Ordering::SeqCst);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "paused", "changed identity pauses");
    assert!(
        error.unwrap().contains("no longer matches"),
        "pause reason identifies the mismatch"
    );
    let posts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND original_id IS NOT NULL",
    )
    .bind(_importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(posts, 0, "nothing imported under the wrong identity");
}

/// An `{"error": "…"}` shaped 200 from `verify_credentials` must not
/// clear the destination profile — identity is validated before any
/// field is trusted, and the category records the failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_shaped_profile_response_never_clears_profile() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "profilesafe").await;
    let importer = account_id(&env.state, "profilesafe").await;
    sqlx::query(
        "UPDATE accounts SET display_name = 'Keep Me', \
         profile_fields = '[{\"name\":\"site\",\"value\":\"https://keep.example\"}]'::jsonb \
         WHERE id = $1",
    )
    .bind(importer)
    .execute(&env.state.db)
    .await
    .unwrap();
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({"connection_id": conn_id, "categories": ["profile"]}),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    // connect + worker verify succeeded; the profile fetch gets an
    // error-shaped success.
    stub.verify_fail_after
        .store(2, std::sync::atomic::Ordering::SeqCst);
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": import_id }))
        .await
        .unwrap();

    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done");
    let cats: Value =
        sqlx::query_scalar("SELECT category_state FROM archive_imports WHERE id = $1")
            .bind(import_id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(
        cats["profile"]["error"].is_string(),
        "profile error recorded: {cats}"
    );

    let (display, fields): (Option<String>, Value) =
        sqlx::query_as("SELECT display_name, profile_fields FROM accounts WHERE id = $1")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(display.as_deref(), Some("Keep Me"), "profile not cleared");
    assert_eq!(fields.as_array().unwrap().len(), 1);
}

/// Cancelling a session settles its pending publication work — rows
/// never linger as "will publish" for a dead import.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_settles_pending_publication() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "cancelpub").await;
    let _importer = account_id(&env.state, "cancelpub").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({
                    "connection_id": conn_id,
                    "categories": ["statuses"],
                    "federate": true,
                }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {body}");
    let import_id: i64 = body["id"].as_str().unwrap().parse().unwrap();

    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": import_id }))
        .await
        .unwrap();
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(pending > 0, "federated import left work rows");

    // Simulate a paused-then-cancelled session — the endpoint settles
    // both the import and its work rows.
    sqlx::query("UPDATE archive_imports SET state = 'paused' WHERE id = $1")
        .bind(import_id)
        .execute(&env.state.db)
        .await
        .unwrap();
    let (status, _) = request(
        &env.app,
        authed(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/yonder/imports/{import_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let leftover: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(leftover, 0, "cancel settled all pending publication work");
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "cancelled");
}

/// Drive one `Import` job pass for a remote session. A due retry is
/// simulated by marking the continuation due, like the delayed
/// envelope firing on schedule.
async fn run_import(env: &Env, import_id: i64) {
    sqlx::query(
        "UPDATE archive_imports SET next_retry_at = now() WHERE id = $1 AND state = 'running'",
    )
    .bind(import_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": import_id }))
        .await
        .unwrap();
}

async fn category_state(env: &Env, import_id: i64) -> Value {
    sqlx::query_scalar("SELECT category_state FROM archive_imports WHERE id = $1")
        .bind(import_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

/// Create a remote session over `conn_id`, returning its import id.
async fn remote_session(env: &Env, token: &str, body: Value) -> i64 {
    let (status, res) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/yonder/import_sessions", body),
            token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "session: {res}");
    res["id"].as_str().unwrap().parse().unwrap()
}

/// A superseded run (another worker claimed the session mid-walk) must
/// leave the owner's state and its pending publication untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn superseded_run_keeps_owner_publication() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "fencedrun").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let import_id = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["statuses"], "federate": true}),
    )
    .await;

    *stub.takeover.lock().await = Some((env.state.db.clone(), import_id));
    run_import(&env, import_id).await;

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(import_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert!(
        pending > 0,
        "the superseded run must not fail the owner's publication"
    );
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(
        state.as_str(),
        "running",
        "the superseded run leaves the session to its owner"
    );
}

/// A 200 that is not JSON (a proxy page) is retried at the same cursor:
/// one bad page recovers on the next run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_page_is_retried_at_its_cursor() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "malformedonce").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let import_id = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["statuses"]}),
    )
    .await;

    stub.malformed_statuses
        .store(1, std::sync::atomic::Ordering::SeqCst);
    run_import(&env, import_id).await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(
        state.as_str(),
        "running",
        "one malformed page is retried, not completed"
    );
    let st = category_state(&env, import_id).await;
    assert_eq!(st["statuses"]["malformed_tries"], json!(1));
    assert!(st["statuses"].get("done").is_none());

    run_import(&env, import_id).await;
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done", "the retry recovers");
    assert!(
        error.is_none(),
        "a recovered category leaves no error: {error:?}"
    );
}

/// Repeated malformed responses for one cursor end the category, and
/// the session says it completed with errors instead of plain `done`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_malformed_pages_complete_with_errors() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "malformedalways").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let import_id = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["statuses"]}),
    )
    .await;

    stub.malformed_statuses
        .store(3, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..3 {
        run_import(&env, import_id).await;
    }
    let (state, error) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done");
    assert_eq!(error.as_deref(), Some("completed with errors: statuses"));
    let st = category_state(&env, import_id).await;
    assert!(
        st["statuses"]["error"].is_string(),
        "category records its failure"
    );
}

/// A transient WebFinger failure while resolving a DM sender fails the
/// category for retry. It is not cached as "not an actor", so the next
/// run resolves the canonical URI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_webfinger_failure_is_retried_not_cached() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "webfingerretry").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let import_id = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["dms"]}),
    )
    .await;

    // Call 0 is the per-run identity check; call 1 is the DM sender.
    stub.webfinger_429_at
        .store(1, std::sync::atomic::Ordering::SeqCst);
    run_import(&env, import_id).await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "running", "rate-limited lookup retries");
    let st = category_state(&env, import_id).await;
    assert!(
        st["_actors"].get("acct:friend@remote.example").is_none(),
        "a transient miss is not cached: {st}"
    );

    run_import(&env, import_id).await;
    let (state, _) = import_state(&env, import_id).await;
    assert_eq!(state.as_str(), "done");
    let st = category_state(&env, import_id).await;
    assert_eq!(
        st["_actors"]["acct:friend@remote.example"],
        json!("https://remote.example/users/friend")
    );
}

/// Observations need the statuses walk in the same session: subjects
/// come from it, so a session without it is refused up front.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observations_require_statuses_in_the_same_session() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "obsnostatuses").await;
    let stub = stub_provider().await;
    let conn_id = connect(&env, &token, &stub.origin).await;
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/yonder/import_sessions",
                json!({"connection_id": conn_id, "categories": ["observations"]}),
            ),
            &token,
        ),
    )
    .await;
    assert!(status.is_client_error(), "refused: {status} {body}");
    assert!(
        body.to_string().contains("statuses"),
        "names the missing category: {body}"
    );
}

/// Merge is the default: an empty remote profile keeps the destination's
/// values. An explicit overwrite lets the empty values replace them.
/// Options report the connection as a decimal string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_merge_keeps_destination_unless_overwrite() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "profilemerge").await;
    let importer = account_id(&env.state, "profilemerge").await;
    sqlx::query("UPDATE accounts SET display_name = 'Keep Me', bio = 'Keep bio' WHERE id = $1")
        .bind(importer)
        .execute(&env.state.db)
        .await
        .unwrap();
    let stub = stub_provider().await;
    stub.empty_profile
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let conn_id = connect(&env, &token, &stub.origin).await;

    let merged = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["profile"]}),
    )
    .await;
    run_import(&env, merged).await;
    let (name, bio): (String, String) =
        sqlx::query_as("SELECT display_name, bio FROM accounts WHERE id = $1")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!((name.as_str(), bio.as_str()), ("Keep Me", "Keep bio"));

    let overwrite = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["profile"], "overwrite_profile": true}),
    )
    .await;
    run_import(&env, overwrite).await;
    let (name, bio): (String, String) =
        sqlx::query_as("SELECT display_name, bio FROM accounts WHERE id = $1")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!((name.as_str(), bio.as_str()), ("", ""));

    let (status, body) = request(
        &env.app,
        authed(
            Request::builder()
                .method("GET")
                .uri(format!("/api/v1/yonder/imports/{overwrite}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["options"]["connection_id"], json!(conn_id.to_string()));
}

/// A callback without the browser binding is refused, and the flow is
/// left intact: the browser that started it can still complete it. The
/// redirect carries a fixed reason and expires the binding cookie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_callback_leaves_flow_intact() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "flowintact").await;
    let stub = stub_provider().await;
    let (state_tok, cookie) = begin_flow(&env, &token, &stub.origin).await;

    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/yonder/import_connections/callback?code=abc&state={state_tok}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let location = res.headers()["location"].to_str().unwrap().to_string();
    assert!(location.ends_with("import_error=forbidden"), "{location}");
    assert!(res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));

    let (status, _) = request(
        &env.app,
        Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/yonder/import_connections/callback?code=abc&state={state_tok}"
            ))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "the rightful browser completes the flow"
    );
    let connections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_connections WHERE origin = $1")
            .bind(&stub.origin)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(connections, 1);
}

/// The same message arriving in two conversation threads is one private
/// record: the second thread merges its participants in and keeps the
/// original sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_dm_threads_merge_recipients() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "dmmerge").await;
    let importer = account_id(&env.state, "dmmerge").await;
    let stub = stub_provider().await;
    stub.duplicate_dm
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let conn_id = connect(&env, &token, &stub.origin).await;
    let import_id = remote_session(
        &env,
        &token,
        json!({"connection_id": conn_id, "categories": ["dms"]}),
    )
    .await;
    run_import(&env, import_id).await;

    let (sender, recipients): (String, Value) = sqlx::query_as(
        "SELECT sender_uri, recipient_uris FROM private_history_messages \
         WHERE owner_id = $1 AND source_uri = 'https://remote.example/statuses/m1'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(sender, "https://remote.example/users/friend");
    let recipients: Vec<&str> = recipients
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        recipients.len(),
        2,
        "both threads' participants: {recipients:?}"
    );
    assert!(recipients.contains(&"https://remote.example/@other"));
    let st = category_state(&env, import_id).await;
    assert_eq!(st["dms"]["merged"], json!(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_import_sanitizes_validates_and_invalidates_cached_actor() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "safeprofile").await;
    let ap = env.state.ap.as_ref().unwrap();
    let _ = activitypub::handler_for_username(ap, "safeprofile")
        .unwrap()
        .unwrap();
    let stub = stub_provider().await;
    *stub.profile_fields.lock().await = Some(json!([
        {"name": "unsafe", "value": "<img src=x onerror=alert(1)>", "verified_at": "2020-01-01"},
        {"name": "blog", "value": "<a href=\"https://blog.example\">My Blog</a>"},
        {"name": "scheme", "value": "<a href=\"javascript:alert(1)\">bad</a>"}
    ]));
    // Prime every document a profile change must drop: the actor doc,
    // this actor's WebFinger response, an outbox page, and the
    // follower collections — plus a second actor's outbox page and
    // WebFinger document that must survive.
    let wf_key =
        pubby_sdk::cache::webfinger_key(&format!("acct:safeprofile@{}", ap.multi.webfinger_domain));
    let other_wf =
        pubby_sdk::cache::webfinger_key(&format!("acct:otheruser@{}", ap.multi.webfinger_domain));
    let outbox_key = pubby_sdk::cache::outbox_key_for("safeprofile", 20, 0);
    let followers_key = pubby_sdk::cache::followers_key_for("safeprofile");
    let actor_key = pubby_sdk::cache::actor_document_key_for("safeprofile");
    let other_outbox = pubby_sdk::cache::outbox_key_for("otheruser", 20, 0);
    for key in [
        &wf_key,
        &outbox_key,
        &followers_key,
        &actor_key,
        &other_outbox,
        &other_wf,
    ] {
        ap.core
            .doc_cache
            .get_or_render::<Value, _>(key.clone(), || Ok(Some(json!({"stale": true}))), None, None)
            .unwrap();
    }
    let connection = connect(&env, &token, &stub.origin).await;
    let id = remote_session(
        &env,
        &token,
        json!({
            "connection_id": connection, "categories": ["profile"]
        }),
    )
    .await;
    run_import(&env, id).await;
    let (_, account) = request(
        &env.app,
        authed(
            Request::builder()
                .uri("/api/v1/accounts/verify_credentials")
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    for field in account["fields"].as_array().unwrap() {
        let value = field["value"].as_str().unwrap();
        assert!(!value.contains("onerror") && !value.contains("javascript:"));
        assert!(field["verified_at"].is_null());
    }
    assert!(account["fields"][1]["value"]
        .as_str()
        .unwrap()
        .contains("My Blog"));
    let doc = activitypub::handler_for_username(ap, "safeprofile")
        .unwrap()
        .unwrap()
        .get_actor_document();
    assert_eq!(doc["name"], json!("Old User"));
    assert!(!doc["attachment"].to_string().contains("onerror"));
    for key in [&actor_key, &wf_key, &outbox_key, &followers_key] {
        assert_eq!(
            ap.core.doc_cache.ttl_remaining(key),
            0.0,
            "stale document survived the profile import"
        );
    }
    assert!(
        ap.core.doc_cache.ttl_remaining(&other_outbox) > 0.0,
        "invalidation must stay scoped to the imported actor"
    );
    assert!(
        ap.core.doc_cache.ttl_remaining(&other_wf) > 0.0,
        "another account's WebFinger document must survive"
    );
    let activities: i64 = sqlx::query_scalar("SELECT count(*) FROM activities")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(
        activities, 0,
        "cache invalidation must not publish an Update"
    );

    *stub.profile_fields.lock().await = Some(json!([{
        "name": "too long",
        "value": "x".repeat(env.state.config().content.max_profile_field_value_length as usize + 1)
    }]));
    let second = remote_session(
        &env,
        &token,
        json!({
            "connection_id": connection, "categories": ["profile"]
        }),
    )
    .await;
    run_import(&env, second).await;
    assert!(category_state(&env, second).await["profile"]["error"].is_string());
    let (_, unchanged) = request(
        &env.app,
        authed(
            Request::builder()
                .uri("/api/v1/accounts/verify_credentials")
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(unchanged["fields"], account["fields"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_session_imports_observations_without_stealing_mappings() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "repeatobs").await;
    let stub = stub_provider().await;
    let connection = connect(&env, &token, &stub.origin).await;
    let first = remote_session(
        &env,
        &token,
        json!({
            "connection_id": connection, "categories": ["statuses"]
        }),
    )
    .await;
    run_import(&env, first).await;
    let second = remote_session(&env, &token, json!({
        "connection_id": connection, "categories": ["statuses", "observations"], "federate": true
    })).await;
    run_import(&env, second).await;
    assert_eq!(import_state(&env, second).await.0, "done");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM historical_observations")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(count > 0);
    let mapping_owner: i64 =
        sqlx::query_scalar("SELECT import_id FROM import_items WHERE kind = 'post' LIMIT 1")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(mapping_owner, first);
    let work: i64 = sqlx::query_scalar("SELECT count(*) FROM publication_work")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(
        work, 0,
        "dedupe must not retroactively publish quiet content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_wins_when_an_inflight_category_returns_unauthorized() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "cancelauth").await;
    let stub = stub_provider().await;
    let connection = connect(&env, &token, &stub.origin).await;
    let id = remote_session(
        &env,
        &token,
        json!({
            "connection_id": connection, "categories": ["statuses"]
        }),
    )
    .await;
    *stub.cancel_on_statuses.lock().await = Some((env.state.db.clone(), id));
    run_import(&env, id).await;
    assert_eq!(import_state(&env, id).await.0, "cancelled");
}

/// A boost that already reached the account over federation has no
/// `original_id` — the imported Announce must dedupe on the reblog
/// index instead of retrying the page on `uix_statuses_reblog`
/// forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_import_dedupes_boosts_received_over_federation() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "boostdup").await;
    let importer = account_id(&env.state, "boostdup").await;
    let stub = stub_provider().await;
    let connection = connect(&env, &token, &stub.origin).await;

    // The boost the stub's outbox announces (reblog target
    // `https://remote.example/statuses/9`) already exists — created
    // by live federation, so `original_id` is NULL.
    let remote_account = seed_remote(&env.state, "https://remote.example/users/other").await;
    let remote_status: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '<p>remote post</p>', 'https://remote.example/statuses/9', \
                'https://remote.example/statuses/9', 'public', false) RETURNING id",
    )
    .bind(remote_account)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO statuses (account_id, content, uri, visibility, reblog_of_id) \
         VALUES ($1, '', $2, 'public', $3)",
    )
    .bind(importer)
    .bind(format!("{BASE}/ap/users/boostdup/statuses/boost9"))
    .bind(remote_status)
    .execute(&env.state.db)
    .await
    .unwrap();

    let id = remote_session(
        &env,
        &token,
        json!({"connection_id": connection, "categories": ["statuses"]}),
    )
    .await;
    run_import(&env, id).await;
    let (state, error) = import_state(&env, id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    let boosts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND reblog_of_id = $2 \
         AND NOT tombstone",
    )
    .bind(importer)
    .bind(remote_status)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(boosts, 1, "the existing boost stands — no duplicate");
    let st = category_state(&env, id).await;
    assert_eq!(st["statuses"]["present"], json!(1));
    assert_eq!(st["statuses"]["applied"], json!(1));
}

/// A boost whose target isn't materialized parks in `pending_boosts`
/// — no empty boost card — and materializes (with its historical
/// timestamp) when the remote object lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_import_defers_boost_until_target_lands() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "deferboost").await;
    let importer = account_id(&env.state, "deferboost").await;
    let stub = stub_provider().await;
    let connection = connect(&env, &token, &stub.origin).await;

    let id = remote_session(
        &env,
        &token,
        json!({"connection_id": connection, "categories": ["statuses"]}),
    )
    .await;
    run_import(&env, id).await;
    let (state, error) = import_state(&env, id).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");

    // No boost row and no stub — the intent is parked, not rendered.
    let boosts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND reblog_of_id IS NOT NULL",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(boosts, 0, "no boost row before its target lands");
    let stubs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE a.username = '_unknown' AND s.uri = 'https://remote.example/statuses/9'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(stubs, 0, "no placeholder stub for the boost target");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pending_boosts WHERE account_id = $1 \
         AND target_uri = 'https://remote.example/statuses/9'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the boost intent is parked");
    let item_state: String = sqlx::query_scalar(
        "SELECT state FROM import_items WHERE account_id = $1 AND kind = 'boost'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(item_state, "pending");

    // The target materializes — the drain writes the boost with the
    // Announce's historical timestamp, not import time.
    let remote_account = seed_remote(&env.state, "https://remote.example/users/other").await;
    let target: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '<p>remote post</p>', 'https://remote.example/statuses/9', \
                'https://remote.example/statuses/9', 'public', false) RETURNING id",
    )
    .bind(remote_account)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let dispatcher = yonder::jobs::Dispatcher::new(&env.state.redis, &env.state.config().jobs);
    yonder::users::transfer::restore::backfill_pending_boosts(
        &env.state.db,
        &dispatcher,
        target,
        "https://remote.example/statuses/9",
    )
    .await
    .unwrap();

    let (boost_id, boost_created, boost_original): (
        i64,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as(
        "SELECT id, created_at, original_created_at FROM statuses \
             WHERE account_id = $1 AND reblog_of_id = $2",
    )
    .bind(importer)
    .bind(target)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        boost_created, boost_original,
        "historical timestamp preserved"
    );
    let cutoff = chrono::DateTime::parse_from_rfc3339("2021-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(
        boost_created < cutoff,
        "the boost keeps the Announce's 2020 date, got {boost_created}"
    );
    let (item_state, mapped): (String, Option<i64>) = sqlx::query_as(
        "SELECT state, target_status_id FROM import_items \
         WHERE account_id = $1 AND kind = 'boost'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!((item_state.as_str(), mapped), ("done", Some(boost_id)));
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pending_boosts WHERE account_id = $1")
            .bind(importer)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(pending, 0, "the parked intent is consumed");
    let reblogs: i32 = sqlx::query_scalar("SELECT reblogs_count FROM statuses WHERE id = $1")
        .bind(target)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(reblogs, 1, "the target's counter moved at materialization");
}

/// A reply whose parent sits outside the walked set resolves its URI
/// through `/api/v1/statuses/{id}` during the walk; a re-import then
/// repairs the already-landed row through the dedupe-hit self-heal,
/// and the unresolved parent gets a `RemoteStatusFetch` so it can
/// materialize and link the reply for real.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_import_links_reply_to_remote_parent() {
    use sha2::Digest;
    use std::sync::atomic::Ordering;

    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".into());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "replylink").await;
    let importer = account_id(&env.state, "replylink").await;
    let stub = stub_provider().await;
    let connection = connect(&env, &token, &stub.origin).await;

    let note_row = |db: &sqlx::PgPool| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(
                "SELECT in_reply_to_uri, in_reply_to_id FROM statuses \
                 WHERE account_id = $1 AND original_id LIKE '%/statuses/101'",
            )
            .bind(importer)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };

    // Phase 1 — no `in_reply_to_id` on the wire: the Note lands as a
    // plain post with no parent reference at all.
    let id1 = remote_session(
        &env,
        &token,
        json!({"connection_id": connection, "categories": ["statuses"]}),
    )
    .await;
    run_import(&env, id1).await;
    let (state, error) = import_state(&env, id1).await;
    assert_eq!(state.as_str(), "done", "import failed: {error:?}");
    assert_eq!(
        note_row(&env.state.db).await,
        (None, None),
        "no parent reference before resolution"
    );

    // Phase 2 — the provider now reports a parent outside the walked
    // set. The re-import hits the dedupe path, but the walk still
    // resolves the parent URI and the self-heal backfills it.
    stub.reply_parent.store(true, Ordering::SeqCst);
    let id2 = remote_session(
        &env,
        &token,
        json!({"connection_id": connection, "categories": ["statuses"]}),
    )
    .await;
    run_import(&env, id2).await;
    let (state, error) = import_state(&env, id2).await;
    assert_eq!(state.as_str(), "done", "re-import failed: {error:?}");

    let parent_uri = format!("{}/statuses/9", stub.origin);
    let (reply_uri, reply_id) = note_row(&env.state.db).await;
    assert_eq!(
        reply_uri.as_deref(),
        Some(parent_uri.as_str()),
        "the parked parent URI was backfilled"
    );
    assert_eq!(reply_id, None, "remote parent not materialized yet");

    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM import_items WHERE account_id = $1 \
         AND source_uri LIKE '%/statuses/101'",
    )
    .bind(importer)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(kind, "reply", "the item reclassified as a reply");

    // The unresolved parent got a fetch so it can materialize and
    // link the reply through `backfill_pending_replies`.
    let hash = hex::encode(sha2::Sha256::digest(parent_uri.as_bytes()));
    let marker = env
        .state
        .redis
        .key(&format!("cache:statusfetch:{}", &hash[..32]));
    let mut conn = env.state.redis.conn().await.unwrap();
    let hit: Option<String> = conn.get(marker).await.unwrap();
    assert!(hit.is_some(), "parent fetch enqueued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_rejects_deleted_or_private_imported_statuses() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "deletedimport").await;
    let owner = account_id(&env.state, "deletedimport").await;
    let id = run_archive_import_opts(
        &env,
        owner,
        &mastodon42_zip(),
        "deleted.zip",
        json!({"federate": true}),
    )
    .await;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT status_id FROM publication_work WHERE import_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&env.state.db)
    .await
    .unwrap();
    assert_eq!(ids.len(), 3);
    let post_ctx = yonder::posts::PostContext::new(&env.state);
    yonder::posts::create::delete_status(&post_ctx, owner, ids[0])
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET visibility = 'followers_only' WHERE id = $1")
        .bind(ids[1])
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET tombstone = true WHERE id = $1")
        .bind(ids[2])
        .execute(&env.state.db)
        .await
        .unwrap();
    transfer::ImportPublishJob::new(env.state.ap.clone().unwrap())
        .run(&job_ctx(&env.state), &json!({"import_id": id}))
        .await
        .unwrap();
    let creates: i64 = sqlx::query_scalar("SELECT count(*) FROM activities WHERE type = 'Create'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(creates, 0);
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_publication_drains_keep_failed_fanout_retryable() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "serializedimport").await;
    let owner = account_id(&env.state, "serializedimport").await;
    let remote = seed_remote(&env.state, "https://remote.example/users/fan").await;
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, inbox, shared_inbox, actor_data) \
         VALUES ($1, $2, 'https://remote.example/users/fan/inbox', '', $3)",
    )
    .bind(remote)
    .bind(owner)
    .bind(json!({
        "id": "https://remote.example/users/fan", "inbox": "https://remote.example/users/fan/inbox"
    }))
    .execute(&env.state.db)
    .await
    .unwrap();
    let id = run_archive_import_opts(
        &env,
        owner,
        &mastodon42_zip(),
        "serialized.zip",
        json!({"federate": true}),
    )
    .await;
    let mut held = env.state.db.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("yonder:import-publication:{id}"))
        .execute(&mut *held)
        .await
        .unwrap();
    let job = transfer::ImportPublishJob::new(env.state.ap.clone().unwrap());
    let ctx = job_ctx(&env.state);
    let payload = json!({"import_id": id});
    job.run(&ctx, &payload).await.unwrap();
    let activities: i64 = sqlx::query_scalar("SELECT count(*) FROM activities")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(
        activities, 0,
        "a drain must respect another worker's database claim"
    );
    held.commit().await.unwrap();
    sqlx::raw_sql(
        "CREATE SEQUENCE fanout_failure_sequence; \
         CREATE FUNCTION fail_first_delivery() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF nextval('fanout_failure_sequence') = 1 THEN \
             RAISE EXCEPTION 'injected ledger failure'; \
         END IF; RETURN NEW; END $$; \
         CREATE TRIGGER fail_delivery BEFORE INSERT ON deliveries \
         FOR EACH ROW EXECUTE FUNCTION fail_first_delivery();",
    )
    .execute(&env.state.db)
    .await
    .unwrap();
    let (a, b) = tokio::join!(job.run(&ctx, &payload), job.run(&ctx, &payload));
    a.unwrap();
    b.unwrap();
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(
        pending, 1,
        "a failed ledger insert cannot be marked delivered"
    );
    sqlx::query("UPDATE publication_work SET next_retry_at = NULL WHERE import_id = $1")
        .bind(id)
        .execute(&env.state.db)
        .await
        .unwrap();
    job.run(&ctx, &payload).await.unwrap();
    let (delivered, deliveries): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'delivered'), \
                (SELECT count(*) FROM deliveries) FROM publication_work WHERE import_id = $1",
    )
    .bind(id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!((delivered, deliveries), (3, 3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_history_media_is_projected_and_owner_authorized() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    let token = register(&env, "historyowner").await;
    let other = register(&env, "historyother").await;
    let owner = account_id(&env.state, "historyowner").await;
    let key = format!("imports/{owner}/test.bin");
    env.state
        .media
        .put(&key, b"private media", "application/octet-stream")
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO private_history_messages (owner_id, sender_uri, source_uri, media_keys) \
         VALUES ($1, 'https://sender.example/user', 'https://sender.example/dm', $2) RETURNING id",
    )
    .bind(owner)
    .bind(json!([
        key,
        "https://source.example/image.png",
        "/etc/passwd",
        format!("imports/{owner}/../escape"),
        "javascript:alert(1)"
    ]))
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (_, list) = request(
        &env.app,
        authed(
            Request::builder()
                .uri("/api/v1/yonder/history/messages")
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    let media = list[0]["media"].as_array().unwrap();
    assert_eq!(media.len(), 2);
    assert_eq!(media[0]["kind"], json!("local"));
    assert_eq!(media[1]["kind"], json!("remote"));
    assert!(!list.to_string().contains(&key));
    let local_url = media[0]["url"].as_str().unwrap();
    let response = env
        .app
        .clone()
        .oneshot(authed(
            Request::builder()
                .uri(local_url)
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        b"private media"[..]
    );
    for index in [-1, 1, 2, 3, 4, 99] {
        let (status, _) = request(
            &env.app,
            authed(
                Request::builder()
                    .uri(format!(
                        "/api/v1/yonder/history/messages/{id}/media/{index}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
                &token,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = request(
        &env.app,
        authed(
            Request::builder()
                .uri(local_url)
                .body(Body::empty())
                .unwrap(),
            &other,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(
        &env.app,
        Request::builder()
            .uri(local_url)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, page) = request(
        &env.app,
        authed(
            Request::builder()
                .uri(format!("/api/v1/yonder/history/messages?max_id={id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ),
    )
    .await;
    assert_eq!(page, json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_drain_waits_for_the_earliest_retry() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "drainwait").await;
    let owner = account_id(&env.state, "drainwait").await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state) \
         VALUES ($1, 'archive', 'merge', 'done') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO publication_work (import_id, activity_type, activity_id, next_retry_at) \
         VALUES ($1, 'Create', 'https://yonder.test/activities/drain-wait', \
                 now() + interval '1 hour')",
    )
    .bind(id)
    .execute(&env.state.db)
    .await
    .unwrap();
    let job = transfer::ImportPublishJob::new(env.state.ap.clone().unwrap());
    let ctx = job_ctx(&env.state);
    job.run(&ctx, &json!({ "import_id": id })).await.unwrap();

    let mut conn = env.state.redis.conn().await.unwrap();
    let delayed: Vec<(String, f64)> = conn
        .zrange_withscores(env.state.redis.key("jobs:delayed"), 0, -1)
        .await
        .unwrap();
    let (_, score) = delayed
        .iter()
        .find(|(member, _)| member.contains(&format!("\"import_id\":{id}")))
        .expect("the drain must re-enqueue itself while work is pending");
    let ahead_ms = score - chrono::Utc::now().timestamp_millis() as f64;
    assert!(
        ahead_ms > 3_000_000.0,
        "a backed-off row must wait for its retry time, got {ahead_ms}ms"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM publication_work WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(pending, 1, "the row must stay pending until its retry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_sweep_requeues_lost_work() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "sweptimport").await;
    let owner = account_id(&env.state, "sweptimport").await;
    // A session whose `Import` envelope was lost after the insert.
    let stuck: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state, created_at) \
         VALUES ($1, 'archive', 'merge', 'pending', now() - interval '5 minutes') \
         RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // A just-created session must not be re-enqueued — its job is on
    // its way.
    let fresh: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state) \
         VALUES ($1, 'archive', 'merge', 'pending') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // A `done` session whose `ImportPublish` enqueue was lost.
    let done_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state) \
         VALUES ($1, 'archive', 'merge', 'done') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO publication_work (import_id, activity_type, activity_id) \
         VALUES ($1, 'Create', 'https://yonder.test/activities/lost-drain')",
    )
    .bind(done_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    // A dead run — `running` with no scheduled retry and no heartbeat
    // for an hour — must be reclaimed.
    let dead: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state, updated_at) \
         VALUES ($1, 'archive', 'merge', 'running', now() - interval '1 hour') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // A live run — same shape but a fresh heartbeat — must not be
    // preempted by a duplicate envelope.
    let live: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state) \
         VALUES ($1, 'archive', 'merge', 'running') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // A paused session's pending work drains on resume, not via sweep.
    let paused: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, state) \
         VALUES ($1, 'archive', 'merge', 'paused') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO publication_work (import_id, activity_type, activity_id) \
         VALUES ($1, 'Create', 'https://yonder.test/activities/paused-drain')",
    )
    .bind(paused)
    .execute(&env.state.db)
    .await
    .unwrap();

    transfer::ImportSweepJob
        .run(&job_ctx(&env.state), &json!({}))
        .await
        .unwrap();

    let mut conn = env.state.redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = conn
        .xrange(env.state.redis.key("queue:bulk"), "-", "+")
        .await
        .unwrap();
    let bodies: Vec<String> = reply
        .ids
        .iter()
        .filter_map(|e| match e.map.get("job") {
            Some(redis::Value::BulkString(b)) => Some(String::from_utf8_lossy(b).to_string()),
            Some(redis::Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let has = |kind: &str, id: i64| {
        bodies.iter().any(|b| {
            b.contains(&format!("\"kind\":\"{kind}\""))
                && b.contains(&format!("\"import_id\":{id}"))
        })
    };
    assert!(
        has("import", stuck),
        "the sweep must re-enqueue a stuck import"
    );
    assert!(
        has("import_publish", done_id),
        "the sweep must drain work whose enqueue was lost"
    );
    assert!(
        !has("import", fresh),
        "a just-created session is not lost work"
    );
    assert!(has("import", dead), "the sweep must reclaim a dead run");
    assert!(!has("import", live), "a live run is not lost work");
    assert!(
        !has("import_publish", paused),
        "a paused session's work drains on resume, not via sweep"
    );
}

/// A duplicate `Import` envelope must not preempt a live run: the
/// claim accepts `running` only when a continuation is due
/// (`next_retry_at`) or the lease has gone stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_claim_refuses_live_run() {
    let _permit = harness::acquire().await;
    let Some(env) = test_env(Config::default()).await else {
        return;
    };
    register(&env, "claimrun").await;
    let owner = account_id(&env.state, "claimrun").await;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports \
         (account_id, kind, mode, state, cancel_requested, run_token) \
         VALUES ($1, 'archive', 'merge', 'running', true, 'live-lease') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();

    // Fresh lease, no due continuation — a duplicate envelope no-ops.
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": id }))
        .await
        .unwrap();
    let (state, token): (String, String) =
        sqlx::query_as("SELECT state, run_token FROM archive_imports WHERE id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), token.as_str()),
        ("running", "live-lease"),
        "a duplicate envelope must not claim a live run"
    );

    // A scheduled backoff is a continuation that is not due yet — a
    // duplicate envelope must not claim early and skip it.
    sqlx::query(
        "UPDATE archive_imports SET next_retry_at = now() + interval '1 hour' WHERE id = $1",
    )
    .bind(id)
    .execute(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": id }))
        .await
        .unwrap();
    let token: String = sqlx::query_scalar("SELECT run_token FROM archive_imports WHERE id = $1")
        .bind(id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(
        token, "live-lease",
        "a duplicate envelope must not skip a scheduled backoff"
    );

    // A due continuation reclaims the session: the lease rotates and
    // the cancel request settles the run.
    sqlx::query(
        "UPDATE archive_imports SET next_retry_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(id)
    .execute(&env.state.db)
    .await
    .unwrap();
    transfer::ImportJob::new(env.state.ap.clone())
        .run(&job_ctx(&env.state), &json!({ "import_id": id }))
        .await
        .unwrap();
    let (state, token): (String, String) =
        sqlx::query_as("SELECT state, run_token FROM archive_imports WHERE id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert_ne!(token, "live-lease", "a due retry must reclaim the run");
    assert_eq!(state, "cancelled");
    let advanced: bool =
        sqlx::query_scalar("SELECT updated_at > created_at FROM archive_imports WHERE id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(advanced, "the claim bumps the run heartbeat");
}

// ---------------------------------------------------------------------------
// Akkoma backups adapter (`/api/v1/pleroma/backups`)
// ---------------------------------------------------------------------------

/// The index projects `archive_exports` in Akkoma's shape and the
/// emitted `url` is a signed, bearer-free link that streams the zip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_backups_projection_and_signed_download() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "backupper").await;
    let owner = account_id(&env.state, "backupper").await;

    env.state
        .media
        .put(
            "exports/backup.zip",
            b"PK\x03\x04payload".as_slice(),
            "application/zip",
        )
        .await
        .unwrap();
    let export_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind, state, file_key, file_size, \
         completed_at, expires_at) VALUES ($1, 'archive', 'done', $2, $3, now(), \
         now() + interval '7 days') RETURNING id",
    )
    .bind(owner)
    .bind("exports/backup.zip")
    .bind(11i64)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // An expired archive is not listed — its signed URL can only 410.
    sqlx::query(
        "INSERT INTO archive_exports (account_id, kind, state, file_key, expires_at) \
         VALUES ($1, 'archive', 'expired', $2, now() - interval '1 day')",
    )
    .bind(owner)
    .bind("exports/old.zip")
    .execute(&env.state.db)
    .await
    .unwrap();

    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/pleroma/backups", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "backups index: {body}");
    let backups = body.as_array().unwrap();
    assert_eq!(backups.len(), 1, "expired archives are not listed: {body}");
    let backup = &backups[0];
    assert_eq!(backup["id"], export_id.to_string());
    assert_eq!(backup["content_type"], "application/zip");
    assert_eq!(backup["file_size"], 11);
    assert_eq!(backup["processed"], true);
    assert!(
        backup["inserted_at"].as_str().unwrap().contains('T'),
        "inserted_at is ISO8601: {body}"
    );

    let url = url::Url::parse(backup["url"].as_str().unwrap()).unwrap();
    let path = format!("{}?{}", url.path(), url.query().unwrap());
    assert!(
        url.path()
            .ends_with(&format!("/api/v1/pleroma/backups/{export_id}/download")),
        "signed url targets the download route: {url}"
    );

    // No bearer: the HMAC-bound {id, expires} pair is the credential.
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "signed download");
    assert_eq!(res.headers().get("cache-control").unwrap(), "no-store");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"PK\x03\x04payload");

    // A disabled owner is refused: the signed link cannot outlive the
    // account checks the bearer route gets from token resolution.
    sqlx::query("UPDATE accounts SET disabled_at = now() WHERE id = $1")
        .bind(owner)
        .execute(&env.state.db)
        .await
        .unwrap();
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "disabled owner cannot download"
    );

    // A pending self-deletion is refused the same way.
    sqlx::query("UPDATE accounts SET disabled_at = NULL WHERE id = $1")
        .bind(owner)
        .execute(&env.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO account_deletions (account_id, scheduled_at) VALUES ($1, now())")
        .bind(owner)
        .execute(&env.state.db)
        .await
        .unwrap();
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "pending-deletion owner cannot download"
    );
}

/// `POST /api/v1/pleroma/backups` queues an archive and renders the
/// refreshed index like upstream; a repeat hits the 7-day cooldown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_backups_create_renders_index_and_cools_down() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "creator").await;

    let (status, body) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/pleroma/backups", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create backup: {body}");
    let backups = body.as_array().unwrap();
    assert_eq!(backups.len(), 1, "create renders the index: {body}");
    assert_eq!(backups[0]["processed"], false, "queued, not processed");

    let res = env
        .app
        .clone()
        .oneshot(authed(
            json_req("POST", "/api/v1/pleroma/backups", json!({})),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "7-day cooldown"
    );
    assert!(
        res.headers().contains_key("retry-after"),
        "cooldown carries a retry hint"
    );
}

/// `read:backups` gates the index, and the signed link fails closed on
/// tampering — it is the only credential on the download route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pleroma_backups_scope_and_link_tampering() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "owner").await;
    let owner = account_id(&env.state, "owner").await;

    // A `read:accounts`-only token cannot reach the backups surface.
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(owner)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO oauth_access_tokens (user_id, token_hash, scopes, expires_at) \
         VALUES ($1, $2, 'read:accounts', now() + interval '1 hour')",
    )
    .bind(user_id)
    .bind(yonder::http::middleware::auth::token_hash("narrow-token"))
    .execute(&env.state.db)
    .await
    .unwrap();
    let (status, _) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/pleroma/backups", json!(null)),
            "narrow-token",
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read:accounts is not read:backups"
    );
    let (status, _) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/pleroma/backups", json!({})),
            "narrow-token",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "create is gated too");

    // Seed a finished export and mint a link through the owner list.
    let export_id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind, state, expires_at) \
         VALUES ($1, 'archive', 'done', now() + interval '7 days') RETURNING id",
    )
    .bind(owner)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/pleroma/backups", json!(null)),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let url = url::Url::parse(body[0]["url"].as_str().unwrap()).unwrap();
    let query = url.query().unwrap();

    // A different export id with the same signature is rejected.
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/pleroma/backups/{}/download?{query}",
                    export_id + 1
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "id is HMAC-bound");

    // A mangled signature is rejected.
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/pleroma/backups/{export_id}/download?{}",
                    query.replace("signature=", "signature=AAAA")
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN, "bad signature");

    // An expired link is rejected on expiry, not on the signature:
    // the error body distinguishes the two refusal paths.
    let (status, body) = request(
        &env.app,
        Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v1/pleroma/backups/{export_id}/download?expires=1&signature=AAAA"
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expired link");
    assert_eq!(body["error"], "download link has expired");
}

/// `return_to` is a local path only: anything else is refused before
/// any remote call is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn begin_refuses_non_local_return_to() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "returnguard").await;
    for bad in [
        "//evil.example",
        "/\\evil.example",
        "https://evil.example/x",
        "/settings?x=1",
    ] {
        let (status, _) = request(
            &env.app,
            authed(
                json_req(
                    "POST",
                    "/api/v1/yonder/import_connections",
                    json!({
                        "origin": "https://old.example",
                        "categories": ["following"],
                        "return_to": bad,
                    }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
}

async fn callback_location(env: &Env, state_tok: &str, cookie: &str) -> String {
    let res = env
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/v1/yonder/import_connections/callback?code=abc&state={state_tok}"
                ))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    res.headers()["location"].to_str().unwrap().to_string()
}

/// The callback returns the browser to the page that started the flow,
/// on success and on failure alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_returns_to_the_starting_page() {
    let _permit = harness::acquire().await;
    let mut config = Config::default();
    config.auth.secret_key = Some("e2e-secret-key-material".to_string());
    let Some(env) = test_env(config).await else {
        return;
    };
    let token = register(&env, "returnback").await;

    let stub = stub_provider().await;
    let (state_tok, cookie) =
        begin_flow_to(&env, &token, &stub.origin, Some("/settings/migration")).await;
    let location = callback_location(&env, &state_tok, &cookie).await;
    assert_eq!(location, "/settings/migration?connected=1");

    let stub = stub_provider().await;
    stub.verify_fail_after
        .store(0, std::sync::atomic::Ordering::SeqCst);
    let (state_tok, cookie) =
        begin_flow_to(&env, &token, &stub.origin, Some("/settings/migration")).await;
    let location = callback_location(&env, &state_tok, &cookie).await;
    assert_eq!(location, "/settings/migration?import_error=invalid");
}
