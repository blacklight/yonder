//! HTTP transport: axum application, routing and middleware.
//!
//! [`build_router`] assembles the modular router — feature epics merge
//! their subrouters under [`api`] without touching this file — and
//! wraps it in the middleware stack (outermost last):
//! request-id → tracing → CORS → routes. [`serve`] binds
//! `server.host`/`server.port` and shuts down gracefully on SIGINT.

pub mod api;
pub mod middleware;
pub mod negotiate;
pub mod oauth;
pub mod openapi;
pub mod ws;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{MatchedPath, Request},
    http::{
        header::{self, HeaderName, HeaderValue},
        Method,
    },
    response::Response,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tower::ServiceBuilder;
use tower_http::{
    classify::ServerErrorsFailureClass,
    cors::CorsLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing::Span;

use crate::{
    cache::{Redis, RedisCache},
    config::{Config, HttpConfig},
    jobs::Dispatcher,
    Result,
};

/// Shared state handed to every route module and middleware.
#[derive(Clone)]
pub struct AppState {
    /// Effective configuration.
    pub config: Arc<Config>,
    /// PostgreSQL pool.
    pub db: PgPool,
    /// Redis handle (pool + key namespace).
    pub redis: Redis,
    /// Namespaced cache over `redis`.
    pub cache: RedisCache,
    /// Background-job enqueue access for handlers.
    pub dispatcher: Dispatcher,
    /// Streaming event bus (epic 16 publishes).
    pub bus: ws::EventBus,
    /// Live `/api/v1/streaming` connections.
    pub connections: ws::ConnectionRegistry,
    /// ActivityPub federation context — `Some` when `federation.enabled`.
    pub ap: Option<crate::activitypub::ApContext>,
}

impl AppState {
    /// Assemble shared state from the resolved config and pools.
    pub fn new(config: Config, db: PgPool, redis: Redis) -> Self {
        let dispatcher = Dispatcher::new(&redis, &config.jobs);
        AppState {
            cache: RedisCache::new(redis.clone()),
            config: Arc::new(config),
            db,
            redis,
            dispatcher,
            bus: ws::EventBus::default(),
            connections: ws::ConnectionRegistry::default(),
            ap: None,
        }
    }
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "meta",
    responses((status = 200, description = "Service is up"))
)]
pub async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Assemble the application router with the full middleware stack.
///
/// Layer order (requests flow top-down): request id → tracing → CORS
/// → routes. `x-request-id` is set before the trace span is built so
/// the id lands in the span, and propagated to the response.
pub fn build_router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .merge(api::router(&state))
        .merge(oauth::router(&state))
        .merge(openapi::router());

    if state.ap.is_some() {
        router = router.merge(crate::activitypub::router(&state));
    }

    if matches!(
        state.config.media.backend,
        crate::config::MediaBackend::Local
    ) {
        router = router.merge(crate::media::storage::router());
    }

    let router = match cors_layer(&state.config.http) {
        Some(cors) => router.layer(cors),
        None => router,
    };

    router
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request| {
                    let request_id = req
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("-")
                        .to_string();
                    let path = req
                        .extensions()
                        .get::<MatchedPath>()
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| req.uri().path().to_string());
                    tracing::info_span!(
                        "request",
                        request_id,
                        method = %req.method(),
                        path,
                        status = tracing::field::Empty,
                    )
                })
                .on_response(|response: &Response, latency: Duration, span: &Span| {
                    span.record(
                        "status",
                        tracing::field::display(response.status().as_u16()),
                    );
                    tracing::info!(
                        parent: span,
                        status = response.status().as_u16(),
                        latency_ms = latency.as_millis(),
                        "response"
                    );
                })
                .on_failure(
                    |error: ServerErrorsFailureClass, latency: Duration, span: &Span| {
                        tracing::error!(
                            parent: span,
                            error = %error,
                            latency_ms = latency.as_millis(),
                            "request failed"
                        );
                    },
                ),
        )
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
        .with_state(state)
}

/// CORS per `http.cors_enabled`/`http.cors_origins` — `None` when
/// disabled (preflights then get no CORS headers).
fn cors_layer(http: &HttpConfig) -> Option<CorsLayer> {
    if !http.cors_enabled || http.cors_origins.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> = http
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                HeaderName::from_static("idempotency-key"),
            ])
            .expose_headers([header::LINK, HeaderName::from_static("x-request-id")]),
    )
}

/// Bind `server.host`/`server.port` and serve until `shutdown`
/// resolves, then drain in-flight connections.
pub async fn serve(
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "listening");
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}
