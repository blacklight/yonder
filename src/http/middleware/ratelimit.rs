//! Redis-backed fixed-window rate limiting.
//!
//! One bucket per named limit (`rate_limit.*` config), keyed
//! `{prefix}:ratelimit:{bucket}:{identity}` where `identity` is
//! `u:{user_id}` for authenticated requests — applying
//! `authenticated_limit` when set — or `ip:{addr}` for anonymous ones.
//! A single Lua round-trip (`INCR` + `PEXPIRE` on first hit, `PTTL`
//! for the reset wait) keeps the counter atomic and hands back the
//! `Retry-After` value.
//!
//! Attach a named limit to a router with [`layer`]:
//!
//! ```ignore
//! router.route_layer(ratelimit::layer(&state, "authentication"))
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::Body,
    http::{header::RETRY_AFTER, Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use redis::Script;
use tower::{Layer, Service};

use crate::{
    config::RateLimit,
    error::ErrorBody,
    http::{
        middleware::auth::{self, MaybeAuth},
        AppState,
    },
};

/// INCR the bucket; seed its expiry on first hit; report the count and
/// ms until reset. One round trip, atomic.
static CHECK_SCRIPT: &str = r#"
local count = redis.call('INCR', KEYS[1])
if count == 1 then
    redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
return {count, redis.call('PTTL', KEYS[1])}
"#;

/// Fixed-window limiter over the app's Redis handle.
#[derive(Clone)]
pub struct RateLimiter {
    state: Arc<AppState>,
}

/// One window check's outcome.
#[derive(Debug)]
pub struct Verdict {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// Seconds until the window resets (`Retry-After`).
    pub retry_after_seconds: u64,
}

impl RateLimiter {
    pub fn new(state: Arc<AppState>) -> Self {
        RateLimiter { state }
    }

    /// Run one request through `bucket`'s window. `auth` selects the
    /// identity and (optionally) the elevated authenticated limit.
    pub async fn check(
        &self,
        bucket: &str,
        limit: &RateLimit,
        auth: Option<&auth::AuthContext>,
        ip: &str,
    ) -> crate::Result<Verdict> {
        let (identity, ceiling) = match auth {
            Some(a) => (
                format!("u:{}", a.user_id),
                limit.authenticated_limit.unwrap_or(limit.limit),
            ),
            None => (format!("ip:{ip}"), limit.limit),
        };
        let key = self
            .state
            .redis
            .key(&format!("ratelimit:{bucket}:{identity}"));
        let mut conn = self.state.redis.conn().await?;
        let (count, pttl): (u32, i64) = Script::new(CHECK_SCRIPT)
            .key(key)
            .arg(limit.period_seconds.saturating_mul(1000))
            .invoke_async(&mut *conn)
            .await?;
        Ok(Verdict {
            allowed: count <= ceiling,
            retry_after_seconds: (pttl.max(0) as u64).div_ceil(1000).max(1),
        })
    }
}

/// The 429 response: `Retry-After` + Mastodon error shape.
fn too_many(retry_after_seconds: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, retry_after_seconds.to_string())],
        Json(ErrorBody {
            error: "Rate limit exceeded".to_string(),
        }),
    )
        .into_response()
}

/// A tower [`Layer`] applying `bucket`'s [`RateLimit`] to a router:
/// `.route_layer(layer(&state, "status"))`.
pub fn layer(state: &Arc<AppState>, bucket: &'static str) -> RateLimitLayer {
    RateLimitLayer {
        state: state.clone(),
        bucket,
    }
}

/// Layer produced by [`layer`].
#[derive(Clone)]
pub struct RateLimitLayer {
    state: Arc<AppState>,
    bucket: &'static str,
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            state: self.state.clone(),
            bucket: self.bucket,
        }
    }
}

/// Service produced by [`RateLimitLayer`].
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    state: Arc<AppState>,
    bucket: &'static str,
}

impl<S> Service<Request<Body>> for RateLimitService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        // Standard tower pattern: the clone services this request so
        // `self.inner` stays ready-polled for the next one.
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);
        let state = self.state.clone();
        let bucket = self.bucket;

        Box::pin(async move {
            let Some(limit) = state.config.rate_limit.bucket(bucket).cloned() else {
                tracing::error!(bucket, "unknown rate-limit bucket");
                return Ok(
                    crate::Error::Internal(format!("unknown rate-limit bucket {bucket}"))
                        .into_response(),
                );
            };
            let token = auth::bearer_token(req.headers(), req.uri());
            let auth = match token {
                Some(t) => auth::resolve_token(&state, &t).await.ok(),
                None => None,
            };
            let ip = super::client_ip::client_ip(
                req.headers(),
                req.extensions(),
                state.config.server.trusted_proxy_hops,
            );
            match RateLimiter::new(state)
                .check(bucket, &limit, auth.as_ref(), &ip)
                .await
            {
                Ok(Verdict { allowed: true, .. }) => {
                    // Cache the resolved auth for downstream extractors.
                    req.extensions_mut().insert(MaybeAuth(auth));
                    inner.call(req).await
                }
                Ok(Verdict {
                    retry_after_seconds,
                    ..
                }) => Ok(too_many(retry_after_seconds)),
                Err(e) => {
                    // Limiter outage shouldn't take the API down.
                    tracing::warn!(error = %e, bucket, "rate limiter check failed; allowing");
                    inner.call(req).await
                }
            }
        })
    }
}
