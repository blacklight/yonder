//! Epic-09 profile tests: `PATCH /api/v1/accounts/update_credentials`
//! across all three transports, validation, field rendering and
//! `rel="me"` verification via `VerifyProfileLinksJob`, avatar upload +
//! serving, and the ActivityPub `Update` fan-out.
//!
//! Same harness conventions as `tests/two_factor.rs`; skipped when no
//! container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use yonder::{
    activitypub,
    config::{Config, RateLimit},
    http::{build_router, AppState},
    jobs::{Dispatcher, JobContext, JobHandler},
};

use crate::harness;

struct Env {
    app: Router,
    state: Arc<AppState>,
}

async fn test_app(mut config: Config) -> Option<Env> {
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
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-profile-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();
    config.federation.base_url = Some("http://127.0.0.1:4000".to_string());

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
    let app = build_router(state.clone());
    Some(Env { app, state })
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.email.smtp_host = None;
    config.federation.enabled = true;
    // The rel="me" fixture pages live on loopback — relaxed fetching by
    // default; SSRF tests set this false before calling `test_app`.
    config.federation.allow_private_targets = true;
    config
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

fn authed(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    Request::from_parts(parts, body)
}

fn json_req(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn form_patch(path: &str, form: &[(&str, &str)]) -> Request<Body> {
    let encoded = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlenc(k), urlenc(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .method("PATCH")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(encoded))
        .unwrap()
}

fn multipart_patch(
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &[u8])>,
) -> Request<Body> {
    let boundary = "TESTBOUNDARY";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((name, filename, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method("PATCH")
        .uri(path)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn token_for(app: &Router, username: &str) -> String {
    let (status, body) = request(
        app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": username, "email": format!("{username}@example.com"),
                "password": "hunter2password", "agreement": true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    body["access_token"].as_str().unwrap().to_string()
}

/// A tiny valid PNG (1x1 red pixel).
fn png_bytes() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

const PATCH: &str = "/api/v1/accounts/update_credentials";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_credentials_json_persists_and_returns_account() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "alice").await;
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({
                    "display_name": "Alice Liddell",
                    "note": "down the <b>rabbit</b> hole",
                    "locked": true,
                    "fields_attributes": [
                        {"name": "site", "value": "https://alice.example"},
                        {"name": "pronouns", "value": "she/her"},
                    ],
                }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_name"], "Alice Liddell");
    assert_eq!(body["locked"], true);
    // Raw in source, rendered in the entity.
    assert_eq!(body["source"]["note"], "down the <b>rabbit</b> hole");
    assert!(body["note"].as_str().unwrap().contains("&lt;b&gt;"));
    let fields = body["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0]["name"], "site");
    // `<a href>` whose text is the full URL — the single-anchor,
    // `href == text` shape Mastodon's remote-field verifier requires.
    assert_eq!(
        fields[0]["value"],
        "<a href=\"https://alice.example\" rel=\"me\">https://alice.example</a>"
    );
    assert!(fields[0].get("value_raw").is_none());
    assert!(fields[0]["verified_at"].is_null());
    assert_eq!(
        body["source"]["fields"][0]["value"],
        "https://alice.example"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_credentials_form_and_empty_clears() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "bob").await;
    let (status, _) = request(
        &env.app,
        authed(
            form_patch(
                PATCH,
                &[
                    ("display_name", "Bob"),
                    ("fields_attributes[0][name]", "blog"),
                    ("fields_attributes[1][name]", "empty-name"),
                    ("fields_attributes[1][value]", "just a value"),
                    ("bot", "true"),
                ],
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // An empty display_name clears the column.
    let (status, body) = request(
        &env.app,
        authed(form_patch(PATCH, &[("display_name", "")]), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["display_name"], "bob");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_credentials_validation_errors() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "carol").await;
    // Over-limit display name → 422.
    let long = "x".repeat(200);
    let (status, _) = request(
        &env.app,
        authed(
            json_req("PATCH", PATCH, json!({ "display_name": long })),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Too many fields → 422.
    let fields: Vec<Value> = (0..11)
        .map(|i| json!({"name": format!("f{i}"), "value": "v"}))
        .collect();
    let (status, _) = request(
        &env.app,
        authed(
            json_req("PATCH", PATCH, json!({ "fields_attributes": fields })),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Huge sparse index → 422.
    let (status, _) = request(
        &env.app,
        authed(
            form_patch(PATCH, &[("fields_attributes[999999][name]", "x")]),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // Unauthenticated → 401.
    let (status, _) = request(&env.app, json_req("PATCH", PATCH, json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A token without `write:accounts` → 403.
    let user_id: i64 = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE a.username = 'carol'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let read_only =
        yonder::auth::tokens::issue_token_pair(&env.state, None, user_id, "read", None, None)
            .await
            .unwrap()
            .access_token;
    let (status, _) = request(
        &env.app,
        authed(
            json_req("PATCH", PATCH, json!({ "display_name": "Nope" })),
            &read_only,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A minimal CRC-32 (PNG uses the same polynomial) for patching chunk
/// CRCs after byte surgery on generated images.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PNG whose IHDR declares `width`×`height` — starts from a real
/// image and rewrites the header (with a valid CRC) so the decoder's
/// dimension-limit path is what rejects it.
fn png_with_dims(width: u32, height: u32) -> Vec<u8> {
    let mut png = png_bytes();
    // Signature(8) + IHDR length(4) + "IHDR"(4); w/h follow at 16..24.
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    let crc = crc32(&png[12..24]); // "IHDR" + 13-byte data
    png[24..28].copy_from_slice(&crc.to_be_bytes());
    png
}

/// A JPEG carrying an `Exif\0\0` APP1 segment spliced after SOI.
fn jpeg_with_exif() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(4, 4, image::Rgb([0, 0, 255]))
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .unwrap();
    let jpeg = out.into_inner();
    assert_eq!(&jpeg[..2], b"\xFF\xD8", "expected SOI");
    let exif = b"Exif\0\0REVIEW-MARKER";
    let mut patched = jpeg[..2].to_vec();
    patched.extend_from_slice(b"\xFF\xE1");
    patched.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    patched.extend_from_slice(exif);
    patched.extend_from_slice(&jpeg[2..]);
    patched
}

/// Read the stored file for a media URL back through the serving route.
async fn fetch_media(env: &Env, url: &str) -> Vec<u8> {
    let path = url.trim_start_matches("http://127.0.0.1:4000");
    let res = env
        .app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_rejects_bombs_oversize_and_strips_exif() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "gwen").await;

    // Declared 60000x60000 → rejected at the dimension limit, before a
    // full decode can allocate (C1).
    let bomb = png_with_dims(60000, 60000);
    let (status, _) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "bomb.png", &bomb))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Over the byte cap → 422 with a size error.
    let big = vec![0u8; (env.state.config().media.max_avatar_size + 1) as usize];
    let (status, _) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "big.png", &big))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // JPEG with a spliced APP1 Exif segment → stored bytes carry no
    // Exif payload (re-encode strips metadata by construction).
    let jpeg = jpeg_with_exif();
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "a.jpg", &jpeg))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored = fetch_media(&env, body["avatar"].as_str().unwrap()).await;
    assert!(
        stored.windows(6).all(|w| w != b"Exif\0\0"),
        "stored image still carries EXIF"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn avatar_upload_stores_serves_and_clears() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "dana").await;
    let png = png_bytes();
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "a.png", &png))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let avatar = body["avatar"].as_str().unwrap();
    assert!(avatar.contains("/media/avatars/"), "avatar url: {avatar}");
    assert!(body["yonder"]["avatar_blurhash"].is_string());
    // The serving route returns the stored bytes.
    let path = avatar.trim_start_matches("http://127.0.0.1:4000");
    let res = env
        .app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/png");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty());
    // Empty file part clears the column.
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "", &[]))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["avatar"], "");
    // Non-image payload → 422.
    let (status, _) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "a.png", b"not an image"))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// Replacing or clearing a profile image defers deletion of the stale
/// object to a `DeleteMediaObject` job — the old URL stays alive while
/// the actor `Update` propagates (serving is `immutable` for a year).
/// The job re-checks the row's reference before deleting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn avatar_replace_and_clear_delete_stale_object() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "erin").await;
    let png = png_bytes();

    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "a.png", &png))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = body["avatar"].as_str().unwrap().to_string();
    let first_key = first.trim_start_matches("http://127.0.0.1:4000/media/");

    // Replace — the old object survives the grace window but a delayed
    // `delete_media_object` envelope is parked for it.
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "b.png", &png))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = body["avatar"].as_str().unwrap().to_string();
    assert_ne!(first, second);
    let second_key = second.trim_start_matches("http://127.0.0.1:4000/media/");
    assert!(
        env.state.media.get(first_key).await.is_ok(),
        "replaced avatar {first_key} must survive the grace delay"
    );
    assert!(env.state.media.get(second_key).await.is_ok());
    let delayed: Vec<String> = redis::cmd("ZRANGE")
        .arg(env.state.redis.key("jobs:delayed"))
        .arg(0)
        .arg(-1)
        .query_async(
            &mut env
                .state
                .redis
                .client()
                .get_multiplexed_async_connection()
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let deletes: Vec<Value> = delayed
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(m).ok())
        .filter(|e| e["kind"] == "delete_media_object")
        .collect();
    assert_eq!(deletes.len(), 1, "{delayed:?}");
    assert_eq!(deletes[0]["payload"]["key"], first_key);

    // The job deletes the object once nothing references it — and
    // leaves the current key alone.
    yonder::media::jobs::DeleteMediaObjectJob
        .run(&job_ctx(&env.state), &json!({ "key": first_key }))
        .await
        .unwrap();
    assert!(
        env.state.media.get(first_key).await.is_err(),
        "replaced avatar {first_key} must be deleted"
    );
    yonder::media::jobs::DeleteMediaObjectJob
        .run(&job_ctx(&env.state), &json!({ "key": second_key }))
        .await
        .unwrap();
    assert!(
        env.state.media.get(second_key).await.is_ok(),
        "still-referenced key must survive the delete job"
    );

    // Clear — the second object is queued for deletion too.
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(PATCH, &[], Some(("avatar", "", &[]))),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["avatar"], "");
    yonder::media::jobs::DeleteMediaObjectJob
        .run(&job_ctx(&env.state), &json!({ "key": second_key }))
        .await
        .unwrap();
    assert!(
        env.state.media.get(second_key).await.is_err(),
        "cleared avatar {second_key} must be deleted"
    );
}

/// Serve `body` on `127.0.0.1:0` — the rel="me" fixture page.
async fn serve_once(body: impl Into<String>) -> String {
    let body = body.into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        for _ in 0..16 {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let mut buf = vec![0u8; 8192];
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if sock.read(&mut buf).await.is_err() {
                continue;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            return;
        }
    });
    format!("http://127.0.0.1:{port}/page")
}

async fn run_verify_job(env: &Env, account_id: i64) {
    yonder::users::profile::VerifyProfileLinksJob
        .run(&job_ctx(&env.state), &json!({ "account_id": account_id }))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rel_me_verification_marks_matching_link() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "erin").await;
    // The page links back to the actor id → verified.
    let page = serve_once(
        "<html><body><a rel=\"me\" href=\"http://127.0.0.1:4000/@erin\">me</a></body></html>",
    )
    .await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": page}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'erin'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_string(), "{fields}");

    // A page that does not link back clears the stamp.
    let bad_page = serve_once("<html><body><a href=\"https://x\">nope</a></body></html>").await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": bad_page}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_null(), "{fields}");
}

/// W6 semantics: an unchanged URL keeps its stamp on PATCH, a changed
/// URL clears it, and a transient fetch failure preserves the stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rel_me_stamp_lifecycle() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "henry").await;
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'henry'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    async fn fields_of(env: &Env, id: i64) -> Value {
        sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_one(&env.state.db)
            .await
            .unwrap()
    }

    // Seed a field and verify it.
    let page = serve_once(
        "<html><body><a rel=\"me\" href=\"http://127.0.0.1:4000/@henry\">me</a></body></html>",
    )
    .await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": page}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    run_verify_job(&env, account_id).await;
    let fields = fields_of(&env, account_id).await;
    let stamp = fields[0]["verified_at"].as_str().unwrap().to_string();
    assert!(!stamp.is_empty());

    // Re-submitting the same field keeps the stamp (W6).
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": page}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fields = fields_of(&env, account_id).await;
    assert_eq!(fields[0]["verified_at"], stamp, "{fields}");

    // A changed URL clears the stamp immediately (no fetch needed).
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": "http://127.0.0.1:1/gone"}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fields = fields_of(&env, account_id).await;
    assert!(fields[0]["verified_at"].is_null(), "{fields}");

    // A transient failure (connection refused on port 1) preserves the
    // previous stamp — seed one directly, then verify against it.
    sqlx::query(
        "UPDATE accounts SET profile_fields = jsonb_set(profile_fields, '{0,verified_at}', \
         to_jsonb('2025-01-01T00:00:00Z'::text)) WHERE id = $1",
    )
    .bind(account_id)
    .execute(&env.state.db)
    .await
    .unwrap();
    run_verify_job(&env, account_id).await;
    let fields = fields_of(&env, account_id).await;
    assert_eq!(fields[0]["verified_at"], "2025-01-01T00:00:00Z", "{fields}");
}

/// With `allow_private_targets = false` the guarded client refuses a
/// loopback field URL — no fetch, no stamp (W5/W10 SSRF).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rel_me_private_target_not_fetched() {
    let mut config = base_config();
    config.federation.allow_private_targets = false;
    let _permit = harness::acquire().await;
    let Some(env) = test_app(config).await else {
        return;
    };
    let token = token_for(&env.app, "irene").await;
    let page = serve_once(
        "<html><body><a rel=\"me\" href=\"http://127.0.0.1:4000/@irene\">me</a></body></html>",
    )
    .await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": page}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'irene'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_null(), "{fields}");
}

/// Serve a mutable actor document at `/users/zed` on loopback — the
/// `RemoteActorFetch` target. Returns the actor's base URL and a
/// handle the test swaps to simulate an actor `Update` refetch.
async fn serve_actor_doc() -> (String, Arc<std::sync::Mutex<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let doc = Arc::new(std::sync::Mutex::new(String::new()));
    let handler_doc = doc.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/users/zed",
                axum::routing::get(move || {
                    let doc = handler_doc.clone();
                    async move {
                        (
                            [("content-type", "application/activity+json")],
                            doc.lock().unwrap().clone(),
                        )
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    (base, doc)
}

/// Remote accounts verify too: `attachment`-sourced fields whose
/// rendered `value` carries an anchor get fetched, and a `rel="me"`
/// backlink to either the actor's `url` or its AP `id` stamps
/// `verified_at`. A re-fetch of the unchanged doc carries the stamp
/// over — a periodic refetch must not flap verified → unverified; a
/// changed field resets it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rel_me_verification_marks_remote_fields() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (base, doc) = serve_actor_doc().await;
    // Page A links back to the actor `url`, page B to the AP `id` —
    // both targets count (Mastodon's uri/url rule).
    let page_a = serve_once(&format!(
        "<html><body><a rel=\"me\" href=\"{base}/@zed\">me</a></body></html>"
    ))
    .await;
    let page_b = serve_once(&format!(
        "<html><body><a rel=\"me\" href=\"{base}/users/zed\">me</a></body></html>"
    ))
    .await;
    let actor_url = format!("{base}/users/zed");
    let doc_for = |attachment: Value| {
        json!({
            "id": actor_url,
            "type": "Person",
            "preferredUsername": "zed",
            "url": format!("{base}/@zed"),
            "inbox": format!("{base}/inbox"),
            "attachment": attachment,
        })
        .to_string()
    };
    *doc.lock().unwrap() = doc_for(json!([
        {"type": "PropertyValue", "name": "Site",
         "value": format!("<a href=\"{page_a}\" rel=\"me\">site</a>")},
        {"type": "PropertyValue", "name": "Blog",
         "value": format!("<a href=\"{page_b}\">blog</a>")},
        {"type": "PropertyValue", "name": "Pronouns", "value": "he/him"},
    ]));

    // The fetch upserts the account and enqueues verification itself.
    let job =
        activitypub::incoming::RemoteActorFetchJob::new(env.state.ap.as_ref().unwrap().clone());
    job.run(&job_ctx(&env.state), &json!({"actor_url": actor_url}))
        .await
        .unwrap();
    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(&actor_url)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_string(), "{fields}");
    assert!(fields[1]["verified_at"].is_string(), "{fields}");
    assert!(fields[2]["verified_at"].is_null(), "{fields}");
    let stamp = fields[0]["verified_at"].clone();

    // A refresh carrying the unchanged doc keeps the stamps.
    job.run(&job_ctx(&env.state), &json!({"actor_url": actor_url}))
        .await
        .unwrap();
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(fields[0]["verified_at"], stamp, "{fields}");
    assert!(fields[1]["verified_at"].is_string(), "{fields}");

    // A changed field value resets the stamp; the next verification
    // pass leaves it null when the new page doesn't link back.
    let bad = serve_once("<html><body>nope</body></html>").await;
    *doc.lock().unwrap() = doc_for(json!([
        {"type": "PropertyValue", "name": "Site",
         "value": format!("<a href=\"{bad}\">site</a>")},
    ]));
    job.run(&job_ctx(&env.state), &json!({"actor_url": actor_url}))
        .await
        .unwrap();
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(fields.as_array().unwrap().len(), 1, "{fields}");
    assert!(fields[0]["verified_at"].is_null(), "{fields}");
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_null(), "{fields}");
}

/// Reading a remote profile with an unverified link field enqueues a
/// `verify_profile_links` job — remote rows ingested before the job
/// existed (or via `attachment`-less partial docs) get their check on
/// first view.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_profile_read_enqueues_field_verification() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let actor_id = "https://remote.example/users/vera";
    let account_id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox, profile_fields) \
         VALUES ('remote.example', 'vera', $1, $2, '', $3::jsonb) RETURNING id",
    )
    .bind(actor_id)
    .bind("https://remote.example/@vera")
    .bind(json!([{
        "name": "Site",
        "value": "<a href=\"https://vera.example\">vera.example</a>",
        "verified_at": Value::Null,
    }]))
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let (status, body) = request(
        &env.app,
        Request::builder()
            .uri(format!("/api/v1/accounts/{account_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let key = env.state.redis.key("queue:bulk");
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
    let enqueued = raw.iter().any(|(_, fields)| {
        fields.iter().any(|f| {
            f.contains("\"verify_profile_links\"")
                && f.contains(&format!("\"account_id\":{account_id}"))
        })
    });
    assert!(enqueued, "no verify_profile_links job queued: {raw:?}");
}

/// A field value submitted as an `<a>` anchor is kept as an anchor
/// (Mastodon accepts anchor markup in field values) and is
/// verifiable; a `javascript:` href degrades to its text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anchor_valued_field_renders_and_verifies() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "kate").await;
    let page = serve_once(
        "<html><body><a rel=\"me\" href=\"http://127.0.0.1:4000/@kate\">me</a></body></html>",
    )
    .await;
    let anchor = format!("<a href=\"{page}\">my site</a>");
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site", "value": anchor}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fields = body["fields"].as_array().unwrap();
    assert_eq!(fields[0]["value"], anchor, "{body}");

    let account_id: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'kate'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    run_verify_job(&env, account_id).await;
    let fields: Value = sqlx::query_scalar("SELECT profile_fields FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert!(fields[0]["verified_at"].is_string(), "{fields}");

    // `javascript:` and markup around the anchor degrade safely.
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "site",
                    "value": "<a href=\"javascript:alert(1)\">x</a><script>alert(2)</script>"}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fields = body["fields"].as_array().unwrap();
    let value = fields[0]["value"].as_str().unwrap();
    assert!(!value.contains("<a"), "{value}");
    assert!(!value.contains("<script"), "{value}");
    assert!(value.contains('x'), "{value}");
    assert!(value.contains("alert(2)"), "{value}");
}

/// A bare `@user@domain` field value renders as a link to
/// `https://{domain}/@{user}` — Mastodon's mention-field shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_valued_field_renders_as_profile_link() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "mia").await;
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "fields_attributes": [{"name": "main",
                    "value": "@mia@mastodon.example"},
                    {"name": "mail", "value": "mia@mastodon.example"}] }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let fields = body["fields"].as_array().unwrap();
    assert_eq!(
        fields[0]["value"], "<a href=\"https://mastodon.example/@mia\">@mia@mastodon.example</a>",
        "{body}"
    );
    // Without the leading `@` the value stays an email-shaped text, not
    // a handle link.
    assert_eq!(fields[1]["value"], "mia@mastodon.example", "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_change_publishes_actor_update() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "frank").await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "PATCH",
                PATCH,
                json!({ "display_name": "Frank N.", "note": "hi <script>alert(1)</script>" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Allow the enqueue/publish a beat — publish runs inline in PATCH.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM activities WHERE object->>'type' = 'Update'")
            .fetch_one(&env.state.db)
            .await
            .unwrap();
    assert!(count >= 1, "expected an Update activity");
    let doc: Value = sqlx::query_scalar(
        "SELECT object FROM activities WHERE object->>'type' = 'Update' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let actor_name = doc["object"]["name"].as_str().unwrap_or_default();
    assert_eq!(actor_name, "Frank N.", "{doc}");
    // The raw `<` in the bio must not reach the AP `summary` unescaped
    // (W1) — `actor_config` renders it via `render_bio_html`.
    let summary = doc["object"]["summary"].as_str().unwrap_or_default();
    assert!(
        summary.contains("&lt;script&gt;") && !summary.contains("<script>"),
        "summary not escaped: {summary}"
    );
}
