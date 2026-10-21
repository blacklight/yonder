//! HTTP integration tests: error mapping, CORS, the Redis rate
//! limiter, OpenAPI serving and the streaming WebSocket — run against
//! real Postgres and Redis containers.
//!
//! Skipped (with a message, never silently green) when no
//! Docker-compatible container runtime is reachable.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use sqlx::PgPool;
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::{Redis as RedisImage, REDIS_PORT};
use tokio::net::TcpListener;
use tower::ServiceExt;
use yonder::cache;
use yonder::config::{Config, RateLimit, RedisConfig};
use yonder::db;
use yonder::http::{self, middleware::auth::token_hash, middleware::ratelimit, AppState};
use yonder::Error;

/// Heuristic check for a reachable container runtime: either
/// `DOCKER_HOST` is set or the default socket exists.
fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

struct Containers {
    _pg: ContainerAsync<Postgres>,
    _redis: ContainerAsync<RedisImage>,
}

/// App state over fresh Postgres + Redis containers, with migrations
/// applied. `tweak` adjusts the config before the state is built.
async fn test_state(tweak: impl FnOnce(&mut Config)) -> Option<(Arc<AppState>, Containers)> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable (set DOCKER_HOST or expose /var/run/docker.sock)");
        return None;
    }
    let pg = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("failed to start postgres container");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("postgres port");
    let db = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{pg_port}/postgres"
    ))
    .await
    .expect("failed to connect to postgres");
    db::migrate::run(&db).await.expect("migrations failed");

    let rc = RedisImage::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("failed to start redis container");
    let redis_port = rc.get_host_port_ipv4(REDIS_PORT).await.expect("redis port");
    let redis = cache::connect(&RedisConfig {
        url: format!("redis://127.0.0.1:{redis_port}/0"),
        ..Default::default()
    })
    .await
    .expect("failed to connect to redis");

    let mut config = Config::default();
    tweak(&mut config);
    let state = Arc::new(AppState::new(config, db, redis));
    Some((
        state,
        Containers {
            _pg: pg,
            _redis: rc,
        },
    ))
}

/// Insert account + user + a valid access token; returns the bearer
/// token. `token_hash` is `hex(sha256(token))` — the epic 07 format.
async fn seed_token(state: &AppState) -> String {
    sqlx::query(
        "INSERT INTO accounts (username, actor_id)
         VALUES ('alice', 'https://yonder.test/ap/actor/alice')",
    )
    .execute(&state.db)
    .await
    .expect("insert account");
    sqlx::query("INSERT INTO users (account_id) VALUES (1)")
        .execute(&state.db)
        .await
        .expect("insert user");
    let token = "test-access-token";
    sqlx::query("INSERT INTO oauth_access_tokens (user_id, token_hash) VALUES (1, $1)")
        .bind(token_hash(token))
        .execute(&state.db)
        .await
        .expect("insert token");
    token.to_string()
}

async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn healthz_and_openapi_served() {
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let app = http::build_router(state);

    let resp = get(&app, "/healthz").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, json!({"status": "ok"}));

    let resp = get(&app, "/openapi.json").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let spec = body_json(resp).await;
    assert!(
        spec["openapi"]
            .as_str()
            .unwrap_or_default()
            .starts_with("3."),
        "expected OpenAPI 3.x, got {}",
        spec["openapi"]
    );
    assert!(spec["paths"]["/healthz"].is_object());
    assert!(spec["paths"]["/api/v1/streaming"].is_object());
}

#[tokio::test]
async fn invalid_bearer_rejected_with_mastodon_shape() {
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let app = http::build_router(state.clone());

    // A protected route using the AuthContext extractor.
    async fn protected(auth: http::middleware::auth::AuthContext) -> String {
        format!("user {}", auth.user_id)
    }
    let app = app.merge(
        axum::Router::new()
            .route("/protected", axum::routing::get(protected))
            .with_state(state.clone()),
    );

    // No token → 401.
    let resp = get(&app, "/protected").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await, json!({"error": "unauthorized"}));

    // Invalid token → 401 with the Mastodon error shape.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(header::AUTHORIZATION, "Bearer bogus-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await, json!({"error": "unauthorized"}));

    // Valid token → 200, AuthContext resolved to the seeded user.
    let token = seed_token(&state).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Streaming's invalid-token rejection needs a real upgrade — the
    // `websocket_rejects_invalid_token_and_scopes_user_stream` test
    // covers it over TCP (oneshot can't perform upgrades).
}

#[tokio::test]
async fn cors_preflight_obeys_origin_policy() {
    let Some((state, _containers)) = test_state(|c| {
        c.http.cors_enabled = true;
        c.http.cors_origins = vec!["https://app.example.com".to_string()];
    })
    .await
    else {
        return;
    };
    let app = http::build_router(state);

    let preflight = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/healthz")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .body(Body::empty())
            .unwrap()
    };

    let resp = app
        .clone()
        .oneshot(preflight("https://app.example.com"))
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://app.example.com"
    );

    let resp = app
        .oneshot(preflight("https://evil.example"))
        .await
        .unwrap();
    assert!(resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

#[tokio::test]
async fn cors_disabled_by_default() {
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let app = http::build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/healthz")
                .header(header::ORIGIN, "https://app.example.com")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

#[tokio::test]
async fn rate_limit_fires_with_retry_after() {
    let Some((state, _containers)) = test_state(|c| {
        c.rate_limit.search = RateLimit {
            limit: 2,
            period_seconds: 60,
            authenticated_limit: None,
        };
    })
    .await
    else {
        return;
    };

    let app = axum::Router::new()
        .route("/probe", axum::routing::get(|| async { "ok" }))
        .route_layer(ratelimit::layer(&state, "search"))
        .with_state(state);

    for _ in 0..2 {
        assert_eq!(get(&app, "/probe").await.status(), StatusCode::OK);
    }
    let resp = get(&app, "/probe").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("Retry-After header");
    assert!((1..=60).contains(&retry_after));
    assert_eq!(
        body_json(resp).await,
        json!({"error": "Rate limit exceeded"})
    );
}

#[tokio::test]
async fn rate_limit_authenticated_override() {
    let Some((state, _containers)) = test_state(|c| {
        c.rate_limit.status = RateLimit {
            limit: 1,
            period_seconds: 60,
            authenticated_limit: Some(4),
        };
    })
    .await
    else {
        return;
    };
    let token = seed_token(&state).await;

    let app = axum::Router::new()
        .route("/probe", axum::routing::get(|| async { "ok" }))
        .route_layer(ratelimit::layer(&state, "status"))
        .with_state(state);

    // Anonymous: limit 1 → second request 429s.
    assert_eq!(get(&app, "/probe").await.status(), StatusCode::OK);
    assert_eq!(
        get(&app, "/probe").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    // Authenticated: separate identity with the elevated limit.
    for i in 0..4 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "request {i}");
    }
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/probe")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn error_mapping_is_mastodon_shaped() {
    async fn not_found() -> Result<&'static str, Error> {
        Err(Error::NotFound("thing".to_string()))
    }
    async fn internal() -> Result<&'static str, Error> {
        Err(Error::Internal("postgres://user:secret@host".to_string()))
    }
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let app = axum::Router::new()
        .route("/nf", axum::routing::get(not_found))
        .route("/ie", axum::routing::get(internal))
        .with_state(state);

    let resp = get(&app, "/nf").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await, json!({"error": "thing not found"}));

    // 5xx errors don't leak internals.
    let resp = get(&app, "/ie").await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body, json!({"error": "Internal server error"}));
}

/// Spawn the real server on an ephemeral port; returns its address.
async fn serve_ephemeral(state: Arc<AppState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = http::build_router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

#[tokio::test]
async fn websocket_echoes_heartbeat_and_streams() {
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let addr = serve_ephemeral(state.clone()).await;
    let url = format!("ws://{addr}/api/v1/streaming");

    let (mut socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("websocket connect");

    // Ping → Pong.
    socket
        .send(tokio_tungstenite::tungstenite::Message::Ping(
            b"hb".to_vec().into(),
        ))
        .await
        .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("pong timeout")
        .unwrap()
        .unwrap();
    assert!(matches!(pong, tokio_tungstenite::tungstenite::Message::Pong(p) if p == b"hb"[..]));

    // Subscribe → bus event arrives only on matching streams.
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"subscribe","stream":"public"}"#.into(),
        ))
        .await
        .unwrap();
    // Let the subscribe frame land before publishing.
    tokio::time::sleep(Duration::from_millis(250)).await;
    state.bus.publish("public", "update", json!({"id": "42"}));
    state.bus.publish("hashtag", "update", json!({"id": "99"}));
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("event timeout")
        .unwrap()
        .unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
        panic!("expected text frame, got {frame:?}");
    };
    let event: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["stream"], json!(["public"]));
    assert_eq!(event["event"], "update");
    assert_eq!(event["payload"], r#"{"id":"42"}"#);

    // User stream without auth → policy close.
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"subscribe","stream":"user"}"#.into(),
        ))
        .await
        .unwrap();
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(f))) => break f,
                Some(Ok(_)) => continue,
                other => panic!("unexpected frame {other:?}"),
            }
        }
    })
    .await
    .expect("close timeout");
    assert_eq!(close.map(|f| f.code), Some(4401.into()));
}

#[tokio::test]
async fn websocket_rejects_invalid_token_and_scopes_user_stream() {
    let Some((state, _containers)) = test_state(|_| {}).await else {
        return;
    };
    let token = seed_token(&state).await;
    let addr = serve_ephemeral(state.clone()).await;

    // Invalid token → HTTP 401, no upgrade.
    let err = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/v1/streaming?access_token=bogus"
    ))
    .await
    .expect_err("bogus token must be rejected");
    assert!(err.to_string().contains("401"), "expected 401, got {err:?}");

    // Valid token via query param → upgrade; user:{uid} events only.
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/v1/streaming?access_token={token}"
    ))
    .await
    .expect("authenticated websocket connect");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"subscribe","stream":"user"}"#.into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    // user_id = 1 (first seeded user); another user's events don't match.
    state.bus.publish("user:9", "notification", json!({"x": 1}));
    state.bus.publish("user:1", "notification", json!({"x": 2}));
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("user event timeout")
        .unwrap()
        .unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
        panic!("expected text frame, got {frame:?}");
    };
    let event: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["stream"], json!(["user:1"]));
}
