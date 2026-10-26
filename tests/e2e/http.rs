//! HTTP integration tests: error mapping, CORS, the Redis rate
//! limiter, OpenAPI serving and the streaming WebSocket — run against
//! real Postgres and Redis containers.
//!
//! Skipped (with a message, never silently green) when no
//! Docker-compatible container runtime is reachable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower::ServiceExt;
use yonder::config::{Config, RateLimit};
use yonder::http::{self, middleware::auth::token_hash, middleware::ratelimit, AppState};
use yonder::Error;

use crate::harness;

/// App state over a fresh per-test database + Redis namespace on the
/// shared containers. `tweak` adjusts the config before the state is
/// built.
async fn test_state(tweak: impl FnOnce(&mut Config)) -> Option<Arc<AppState>> {
    let infra = harness::infra().await?;
    let mut config = Config::default();
    tweak(&mut config);
    let slug = harness::test_slug();
    let db = harness::provision_db(infra, &slug).await;
    let redis = harness::scoped_redis(infra, &mut config.redis, &slug).await;
    Some(Arc::new(AppState::new(config, db, redis)))
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|c| {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|c| {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|c| {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
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
    // Mastodon wire format carries the subscription name, not the
    // internal scoped key.
    assert_eq!(event["stream"], json!(["user"]));
}

#[tokio::test]
async fn websocket_pre_subscribes_via_query_and_folds_params() {
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
        return;
    };
    let token = seed_token(&state).await;
    let addr = serve_ephemeral(state.clone()).await;

    // `?stream=` requires auth for private streams even on the
    // upgrade request.
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/streaming?stream=user"))
        .await
        .expect_err("anonymous ?stream=user must be rejected");
    assert!(err.to_string().contains("401"), "expected 401, got {err:?}");

    // Authenticated pre-subscription receives scoped events as ["user"].
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/api/v1/streaming?access_token={token}&stream=user"
    ))
    .await
    .expect("websocket connect with ?stream=user");
    tokio::time::sleep(Duration::from_millis(250)).await;
    state.bus.publish("user:1", "update", json!({"x": 1}));
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("user event timeout")
        .unwrap()
        .unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
        panic!("expected text frame, got {frame:?}");
    };
    let event: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["stream"], json!(["user"]));

    // Parameterized subscribe: {"stream":"hashtag","tag":"rust"} →
    // events publish on `hashtag:rust` and arrive as
    // ["hashtag","rust"].
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"subscribe","stream":"hashtag","tag":"rust"}"#.into(),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    state
        .bus
        .publish("hashtag:rust", "update", json!({"id": "1"}));
    state
        .bus
        .publish("hashtag:go", "update", json!({"id": "2"}));
    let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("hashtag event timeout")
        .unwrap()
        .unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
        panic!("expected text frame, got {frame:?}");
    };
    let event: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(event["stream"], json!(["hashtag", "rust"]));
}

#[tokio::test]
async fn database_outage_is_500_not_401() {
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|_| {}).await else {
        return;
    };
    let token = seed_token(&state).await;

    async fn protected(auth: http::middleware::auth::AuthContext) -> String {
        format!("user {}", auth.user_id)
    }
    let app = http::build_router(state.clone()).merge(
        axum::Router::new()
            .route("/protected", axum::routing::get(protected))
            .with_state(state.clone()),
    );

    // Kill the database pool: token resolution hits infrastructure
    // failure, which must surface as 5xx — never 401 (Mastodon clients
    // log the user out on 401).
    state.db.close().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/protected")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "infra failure must not look like an auth failure"
    );
    assert_eq!(
        body_json(resp).await,
        json!({"error": "Internal server error"})
    );
}

/// Scope/state gate refusals name the failing gate in the body —
/// a bare "forbidden" leaves the SPA unable to say *why* a write
/// was refused (unconfirmed email, missing scope, pending approval).
#[tokio::test]
async fn forbidden_names_the_gate() {
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|c| {
        c.email.smtp_host = Some("127.0.0.1".into());
    })
    .await
    else {
        return;
    };
    let app = http::build_router(state.clone());
    let token = seed_token(&state).await;

    let post_status = |token: &str| {
        let app = app.clone();
        let token = token.to_string();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/statuses")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // The seeded token carries the default 'read' scope — a write
    // endpoint refuses it on coverage first.
    let resp = post_status(&token).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await["error"],
        json!("This action is outside the authorized scopes")
    );

    // Broaden the token and approve the account — the email gate is
    // the next refusal, and it names itself as well.
    sqlx::query("UPDATE oauth_access_tokens SET scopes = 'read write'")
        .execute(&state.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET approved_at = now()")
        .execute(&state.db)
        .await
        .unwrap();
    let resp = post_status(&token).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await["error"],
        json!("You must confirm your email address before you can perform this action")
    );
}

/// Regression: the `authentication` bucket must cover only the
/// auth-adjacent routes — the blanket `route_layer` used to wrap
/// nearly the whole API, so a page refresh could 429
/// `verify_credentials`/`instance`/settings at the 5/min default.
#[tokio::test]
async fn authentication_bucket_scoped_to_auth_endpoints() {
    let _permit = harness::acquire().await;
    let Some(state) = test_state(|c| {
        c.rate_limit.authentication = RateLimit {
            limit: 1,
            period_seconds: 60,
            authenticated_limit: Some(3),
        };
    })
    .await
    else {
        return;
    };
    let app = http::build_router(state.clone());

    // Routine reads that used to be swept into the bucket stay free:
    // both calls pass instead of the second eating a 429.
    assert_eq!(get(&app, "/api/v1/instance").await.status(), StatusCode::OK);
    assert_eq!(get(&app, "/api/v1/instance").await.status(), StatusCode::OK);

    // The bucket still guards auth endpoints. Anonymous callers get
    // `limit` (1): the first call reaches the handler (401), the
    // second is throttled.
    let resp = get(&app, "/api/v1/apps/verify_credentials").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = get(&app, "/api/v1/apps/verify_credentials").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // A signed-in client gets `authenticated_limit` (3) instead —
    // verify_credentials on every app boot no longer trips the
    // anti-brute-force ceiling.
    let token = seed_token(&state).await;
    for i in 0..3 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/tokens")
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
                .uri("/api/v1/tokens")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}
