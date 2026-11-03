//! Account-archive restoration tests: real Mastodon (ZIP + pre-4.2
//! tar.gz), Akkoma and Pleroma archive shapes built in memory, driven
//! through the import job — plus archive-security rejection cases and
//! export-fidelity checks.
//!
//! The archive restoration engine and restricted-export sessions are
//! implemented, so these run for real. Remaining `#[ignore]`d tests
//! pin Sections 8–10 (federation toggle, remote API import, overlays)
//! as failing targets for the next phase.
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
// Section 8–10 targets — un-ignored as those sections land.
// ---------------------------------------------------------------------------

/// `federate: false` imports must produce zero ActivityPub output,
/// notifications, or fan-out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 8: configurable import publication"]
async fn import_federation_off_emits_no_activities() {
    todo!("section 8")
}

/// `federate: true` delivers each authored activity exactly once via
/// the publication-work drain and the delivery ledger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 8: configurable import publication"]
async fn import_federation_on_delivers_once() {
    todo!("section 8")
}

/// A remote OAuth import honors caps/order and resumes mid-page after
/// a simulated worker kill; 401s pause for reauthorization.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 9: remote OAuth and API import"]
async fn remote_oauth_import_resumes_after_kill() {
    todo!("section 9")
}

/// Received likes/boosts/replies land as owner-scoped observations —
/// never as live interactions on the imported copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "section 10: historical overlays"]
async fn historical_overlays_stay_owner_scoped() {
    todo!("section 10")
}
