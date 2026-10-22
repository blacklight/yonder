//! Epic-10 user-settings tests: BOOTSTRAP defaults on a fresh account,
//! Mastodon-native setters on `update_credentials`, the yonder
//! settings endpoint, strict validation, atomicity, and effective
//! language resolution (`Accept-Language` + `i18n.fallback_locale`).
//!
//! Same harness conventions as `tests/profile.rs`; skipped when no
//! container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::{path::Path, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::{postgres::Postgres, redis::Redis};
use tower::ServiceExt;
use yonder::{
    activitypub, cache,
    config::{Config, RateLimit},
    db,
    http::{build_router, AppState},
    jobs::Dispatcher,
};

fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

struct Env {
    app: Router,
    _state: Arc<AppState>,
    _pg: testcontainers::ContainerAsync<Postgres>,
    _redis: testcontainers::ContainerAsync<Redis>,
}

async fn test_app(mut config: Config) -> Option<Env> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return None;
    }
    let pg = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.unwrap();
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
    ))
    .await
    .unwrap();
    db::migrate::run(&pool).await.unwrap();

    let redis = Redis::default().start().await.expect("redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.unwrap();
    config.redis.url = format!("redis://127.0.0.1:{redis_port}");
    let redis_handle = cache::connect(&config.redis).await.unwrap();

    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();
    config.media.local_path = std::env::temp_dir()
        .join(format!("yonder-settings-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();
    config.federation.base_url = Some("http://127.0.0.1:4000".to_string());

    let dispatcher = Dispatcher::new(&redis_handle, &config.jobs);
    let ap = activitypub::build(&config, &pool, &redis_handle, &dispatcher)
        .await
        .unwrap();
    let mut state = AppState::new(config, pool, redis_handle);
    state.ap = Some(ap);
    let state = Arc::new(state);
    let app = build_router(state.clone());
    Some(Env {
        app,
        _state: state,
        _pg: pg,
        _redis: redis,
    })
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.email.smtp_host = None;
    config.federation.enabled = true;
    config.federation.allow_private_targets = true;
    config
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

fn multipart_patch(path: &str, fields: &[(&str, &str)]) -> Request<Body> {
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

async fn register(app: &Router, username: &str, extra: Value) -> (StatusCode, Value) {
    let mut payload = serde_json::Map::new();
    payload.insert("username".into(), username.into());
    payload.insert("email".into(), format!("{username}@example.com").into());
    payload.insert("password".into(), "hunter2password".into());
    payload.insert("agreement".into(), true.into());
    if let Value::Object(map) = extra {
        payload.extend(map);
    }
    request(app, json_req("POST", "/api/v1/accounts", payload.into())).await
}

async fn token_for(app: &Router, username: &str) -> String {
    let (status, body) = register(app, username, json!({})).await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    body["access_token"].as_str().unwrap().to_string()
}

async fn verify(app: &Router, token: &str) -> Value {
    let (status, body) = request(
        app,
        authed(
            Request::builder()
                .method("GET")
                .uri("/api/v1/accounts/verify_credentials")
                .body(Body::empty())
                .unwrap(),
            token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "verify_credentials: {body}");
    body
}

async fn get_json(app: &Router, token: &str, path: &str) -> (StatusCode, Value) {
    request(
        app,
        authed(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
            token,
        ),
    )
    .await
}

async fn patch_json(app: &Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    request(app, authed(json_req("PATCH", path, body), token)).await
}

const UPDATE: &str = "/api/v1/accounts/update_credentials";
const SETTINGS: &str = "/api/v1/yonder/settings";
const PREFS: &str = "/api/v1/preferences";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_account_reports_bootstrap_defaults() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "defaults").await;
    let body = verify(&env.app, &token).await;

    assert_eq!(body["source"]["privacy"], "public");
    assert_eq!(body["source"]["sensitive"], false);
    assert_eq!(body["source"]["language"], "en");
    assert_eq!(body["hide_collections"], false);
    assert_eq!(body["indexable"], true);
    assert_eq!(body["noindex"], false);
    assert_eq!(body["locked"], false);
    assert_eq!(body["bot"], false);
    assert_eq!(body["discoverable"], true);

    let s = &body["yonder"]["settings"];
    assert_eq!(s["interface_language"], Value::Null);
    assert_eq!(s["default_posting_language"], Value::Null);
    assert_eq!(s["default_visibility"], "public");
    assert_eq!(s["default_sensitive"], false);
    assert_eq!(s["theme"], "system");
    assert_eq!(s["accent"], Value::Null);
    assert_eq!(s["block_stranger_notifications"], false);
    assert_eq!(s["scheduled_deletion_days"], 0);
    assert_eq!(s["show_app_display"], true);
    assert_eq!(s["include_public_posts_in_search"], true);
    assert_eq!(s["directory_opt_in"], true);
    assert_eq!(s["cw_always_expand"], false);
    assert_eq!(s["filter_languages"], json!([]));
    assert_eq!(
        s["email_notifications"],
        json!({
            "follow": false, "follow_request": true, "reblog": false,
            "favourite": false, "mention": true, "quote": true,
        })
    );
    assert_eq!(s["effective_posting_language"], "en");
    // The blurhash keys the yonder object already carried survive.
    assert!(body["yonder"].get("avatar_blurhash").is_some());
    assert!(body["yonder"].get("header_blurhash").is_some());

    // The Mastodon preferences view agrees.
    let (status, prefs) = get_json(&env.app, &token, PREFS).await;
    assert_eq!(status, StatusCode::OK, "{prefs}");
    assert_eq!(prefs["posting:default:visibility"], "public");
    assert_eq!(prefs["posting:default:sensitive"], false);
    assert_eq!(prefs["posting:default:language"], "en");
    assert_eq!(prefs["reading:expand:media"], "default");
    assert_eq!(prefs["reading:expand:spoilers"], false);

    // GET /api/v1/yonder/settings returns the same object.
    let (status, settings) = get_json(&env.app, &token, SETTINGS).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings, *s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_credentials_native_settings_all_transports() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "native").await;

    // Form transport (bracket source keys).
    let (status, body) = request(
        &env.app,
        authed(
            form_patch(
                UPDATE,
                &[
                    ("locked", "1"),
                    ("hide_collections", "true"),
                    ("indexable", "0"),
                    ("source[privacy]", "unlisted"),
                    ("source[language]", "IT"),
                    ("source[sensitive]", "true"),
                ],
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["locked"], true);
    assert_eq!(body["hide_collections"], true);
    assert_eq!(body["indexable"], false);
    assert_eq!(body["source"]["privacy"], "unlisted");
    assert_eq!(body["source"]["language"], "it");
    assert_eq!(body["source"]["sensitive"], true);
    assert_eq!(body["noindex"], true);

    // JSON transport (nested source object) — `hide_collections: null`
    // resets to the default.
    let (status, body) = patch_json(
        &env.app,
        &token,
        UPDATE,
        json!({
            "hide_collections": null,
            "indexable": null,
            "source": {"privacy": "private", "language": "de", "sensitive": false},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hide_collections"], false);
    assert_eq!(body["indexable"], true);
    assert_eq!(body["source"]["privacy"], "private");
    assert_eq!(body["source"]["language"], "de");
    assert_eq!(body["source"]["sensitive"], false);
    assert_eq!(body["noindex"], false);

    // Multipart transport — empty scalars reset.
    let (status, body) = request(
        &env.app,
        authed(
            multipart_patch(
                UPDATE,
                &[
                    ("hide_collections", "1"),
                    ("source[privacy]", ""),
                    ("source[language]", ""),
                    ("locked", ""),
                ],
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["locked"], false);
    assert_eq!(body["hide_collections"], true);
    assert_eq!(body["source"]["privacy"], "public");
    assert_eq!(body["source"]["language"], "en");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yonder_settings_set_read_unset() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "yonder").await;

    let (status, body) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({
            "interface_language": "ES",
            "default_posting_language": "ar",
            "default_visibility": "followers_only",
            "default_sensitive": true,
            "theme": "dark",
            "accent": "#A1B2C3",
            "block_stranger_notifications": true,
            "scheduled_deletion_days": 30,
            "show_app_display": false,
            "include_public_posts_in_search": false,
            "directory_opt_in": false,
            "cw_always_expand": true,
            "filter_languages": ["IT", "de-AT", "it"],
            "email_notifications": {"follow": true, "mention": false},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["interface_language"], "es");
    assert_eq!(body["default_posting_language"], "ar");
    // Stored `followers_only` renders as Mastodon's `private` on the wire.
    assert_eq!(body["default_visibility"], "private");
    assert_eq!(body["default_sensitive"], true);
    assert_eq!(body["theme"], "dark");
    assert_eq!(body["accent"], "#a1b2c3");
    assert_eq!(body["block_stranger_notifications"], true);
    assert_eq!(body["scheduled_deletion_days"], 30);
    assert_eq!(body["show_app_display"], false);
    assert_eq!(body["include_public_posts_in_search"], false);
    assert_eq!(body["directory_opt_in"], false);
    assert_eq!(body["cw_always_expand"], true);
    assert_eq!(body["filter_languages"], json!(["it", "de-at"]));
    assert_eq!(body["email_notifications"]["follow"], true);
    assert_eq!(body["email_notifications"]["mention"], false);
    // Untouched email prefs keep their defaults.
    assert_eq!(body["email_notifications"]["quote"], true);

    // Persisted: a fresh read agrees, and the Mastodon views reflect it.
    let (status, again) = get_json(&env.app, &token, SETTINGS).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, body);
    let cred = verify(&env.app, &token).await;
    assert_eq!(cred["source"]["privacy"], "private");
    assert_eq!(cred["source"]["language"], "ar");
    assert_eq!(cred["source"]["sensitive"], true);
    let (status, prefs) = get_json(&env.app, &token, PREFS).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(prefs["posting:default:visibility"], "private");
    assert_eq!(prefs["posting:default:language"], "ar");
    assert_eq!(prefs["posting:default:sensitive"], true);
    assert_eq!(prefs["reading:expand:spoilers"], true);

    // Unset: null resets every field to the BOOTSTRAP default.
    let (status, body) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({
            "interface_language": null,
            "default_posting_language": null,
            "default_visibility": null,
            "default_sensitive": null,
            "theme": null,
            "accent": null,
            "block_stranger_notifications": null,
            "scheduled_deletion_days": null,
            "show_app_display": null,
            "include_public_posts_in_search": null,
            "directory_opt_in": null,
            "cw_always_expand": null,
            "filter_languages": null,
            "email_notifications": null,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["interface_language"], Value::Null);
    assert_eq!(body["default_posting_language"], Value::Null);
    assert_eq!(body["default_visibility"], "public");
    assert_eq!(body["default_sensitive"], false);
    assert_eq!(body["theme"], "system");
    assert_eq!(body["accent"], Value::Null);
    assert_eq!(body["block_stranger_notifications"], false);
    assert_eq!(body["scheduled_deletion_days"], 0);
    assert_eq!(body["show_app_display"], true);
    assert_eq!(body["include_public_posts_in_search"], true);
    assert_eq!(body["directory_opt_in"], true);
    assert_eq!(body["cw_always_expand"], false);
    assert_eq!(body["filter_languages"], json!([]));
    assert_eq!(
        body["email_notifications"],
        json!({
            "follow": false, "follow_request": true, "reblog": false,
            "favourite": false, "mention": true, "quote": true,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_validation_errors() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "strict").await;

    let bad: Vec<Value> = vec![
        json!({"bogus": 1}),
        json!({"theme": "sepia"}),
        json!({"accent": "red"}),
        json!({"accent": "#12345"}),
        json!({"default_visibility": "direct"}),
        json!({"default_visibility": "everyone"}),
        json!({"interface_language": "fr"}),
        json!({"default_posting_language": "english"}),
        json!({"scheduled_deletion_days": -1}),
        json!({"scheduled_deletion_days": 3651}),
        json!({"scheduled_deletion_days": "soon"}),
        json!({"cw_always_expand": "yes"}),
        json!({"block_stranger_notifications": 1}),
        json!({"filter_languages": "it"}),
        json!({"filter_languages": ["not a tag!"]}),
        json!({"email_notifications": "all"}),
        json!({"email_notifications": {"zzz": true}}),
        json!([]),
    ];
    for body in bad {
        let (status, resp) = patch_json(&env.app, &token, SETTINGS, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {resp}");
    }

    // Over the 100-tag filter_languages bound → 422.
    let oversized: Vec<String> = (0..=100).map(|i| format!("t{i:02}")).collect();
    let (status, resp) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({"filter_languages": oversized}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{resp}");

    // update_credentials source.* validation.
    for body in [
        json!({"source": {"privacy": "direct"}}),
        json!({"source": {"language": "fr"}}),
        json!({"source": {"privacy": "public"}, "source_privacy": "private"}),
        json!({"source": {"unexpected": true}}),
        json!({"hide_collections": "maybe"}),
    ] {
        let (status, resp) = patch_json(&env.app, &token, UPDATE, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {resp}");
    }
    let (status, resp) = request(
        &env.app,
        authed(form_patch(UPDATE, &[("source[privacy]", "direct")]), &token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{resp}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_do_not_leak_between_users() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let alice = token_for(&env.app, "alice").await;
    let (status, _) = patch_json(
        &env.app,
        &alice,
        SETTINGS,
        json!({
            "theme": "dark",
            "interface_language": "it",
            "email_notifications": {"follow": true},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A second user's credential view shows only their own defaults —
    // none of alice's private settings surface there.
    let bob = token_for(&env.app, "bob").await;
    let body = verify(&env.app, &bob).await;
    assert_eq!(body["source"]["language"], "en");
    let s = &body["yonder"]["settings"];
    assert_eq!(s["interface_language"], Value::Null);
    assert_eq!(s["theme"], "system");
    assert_eq!(s["email_notifications"]["follow"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_direct_default_visibility_renders_private() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "legacy").await;
    // The epic-02 CHECK admits `direct`; simulate a row written before
    // the API rejected it — reads must not crash and render `private`.
    sqlx::query(
        "UPDATE accounts SET default_visibility = 'direct' \
         WHERE username = 'legacy' AND domain IS NULL",
    )
    .execute(&env._state.db)
    .await
    .unwrap();
    let body = verify(&env.app, &token).await;
    assert_eq!(body["source"]["privacy"], "private");
    assert_eq!(body["yonder"]["settings"]["default_visibility"], "private");
    let (status, prefs) = get_json(&env.app, &token, PREFS).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(prefs["posting:default:visibility"], "private");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_credentials_settings_are_atomic() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "atomic").await;

    // A valid profile edit plus an invalid settings value must fail
    // together — nothing persists.
    let (status, _) = patch_json(
        &env.app,
        &token,
        UPDATE,
        json!({"display_name": "Should Not Stick", "source": {"privacy": "bogus"}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let body = verify(&env.app, &token).await;
    assert_eq!(body["display_name"], "atomic");
    assert_eq!(body["source"]["privacy"], "public");

    // And a settings-only PATCH must not disturb unrelated fields.
    let (status, _) = patch_json(
        &env.app,
        &token,
        UPDATE,
        json!({"display_name": "Atomic Alice", "locked": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = patch_json(&env.app, &token, UPDATE, json!({"bot": true})).await;
    assert_eq!(status, StatusCode::OK);
    let body = verify(&env.app, &token).await;
    assert_eq!(body["display_name"], "Atomic Alice");
    assert_eq!(body["locked"], true);
    assert_eq!(body["bot"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effective_language_resolution() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let token = token_for(&env.app, "polyglot").await;

    // Stored interface language drives an unset posting language.
    let (status, _) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({"interface_language": "it"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = verify(&env.app, &token).await;
    assert_eq!(body["source"]["language"], "it");

    // An explicit posting language wins.
    let (status, _) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({"default_posting_language": "es"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = verify(&env.app, &token).await;
    assert_eq!(body["source"]["language"], "es");

    // System mode (both unset): Accept-Language negotiation with
    // region fallback, then the instance fallback.
    let (status, _) = patch_json(
        &env.app,
        &token,
        SETTINGS,
        json!({"interface_language": null, "default_posting_language": null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let req = authed(
        Request::builder()
            .method("GET")
            .uri("/api/v1/accounts/verify_credentials")
            .header("accept-language", "fr-CA,fr;q=0.9,es-MX;q=0.8")
            .body(Body::empty())
            .unwrap(),
        &token,
    );
    let (status, body) = request(&env.app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"]["language"], "es");
    // No usable client language → fallback_locale.
    let body = verify(&env.app, &token).await;
    assert_eq!(body["source"]["language"], "en");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_fallback_locale_applies() {
    let mut config = base_config();
    config.i18n.fallback_locale = "it".to_string();
    let Some(env) = test_app(config).await else {
        return;
    };
    let token = token_for(&env.app, "fallback").await;
    let body = verify(&env.app, &token).await;
    assert_eq!(body["source"]["language"], "it");
    assert_eq!(
        body["yonder"]["settings"]["effective_posting_language"],
        "it"
    );
    // A client Accept-Language still wins over the fallback.
    let req = authed(
        Request::builder()
            .method("GET")
            .uri("/api/v1/accounts/verify_credentials")
            .header("accept-language", "de-DE")
            .body(Body::empty())
            .unwrap(),
        &token,
    );
    let (_, body) = request(&env.app, req).await;
    assert_eq!(body["source"]["language"], "de");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_validates_locale() {
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, _) = register(&env.app, "badlocale", json!({"locale": "fr"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = register(&env.app, "goodlocale", json!({"locale": "IT"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap();
    let body = verify(&env.app, token).await;
    assert_eq!(body["yonder"]["settings"]["interface_language"], "it");
    assert_eq!(body["source"]["language"], "it");
}
