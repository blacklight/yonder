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
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

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
        middleware::auth::{self, TokenResolution},
        AppState,
    },
};

/// INCR the bucket; seed its expiry on first hit; report the count and
/// ms until reset. One round trip, atomic. `LazyLock` keeps the script
/// SHA-1 cached across requests.
static CHECK_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r#"
local count = redis.call('INCR', KEYS[1])
if count == 1 then
    redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
return {count, redis.call('PTTL', KEYS[1])}
"#,
    )
});

/// Budget for one limiter check — long enough for a healthy Redis,
/// short enough that an outage fails open quickly rather than stalling
/// every request on the pool's connection timeout.
const CHECK_TIMEOUT: Duration = Duration::from_millis(250);

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
        let (count, pttl): (u32, i64) = CHECK_SCRIPT
            .clone()
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
///
/// # Panics
/// When `bucket` isn't a known `rate_limit.*` key — a programming
/// error caught at router build time, not a runtime 500.
pub fn layer(state: &Arc<AppState>, bucket: &'static str) -> RateLimitLayer {
    assert!(
        state.config.rate_limit.bucket(bucket).is_some(),
        "unknown rate-limit bucket {bucket}"
    );
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
            // Resolution errors (database down) are not cached: the
            // request counts against the anonymous IP bucket, and a
            // downstream `AuthContext` re-resolves and surfaces the
            // real error rather than a spurious 401.
            let resolution = auth::resolve_request(&state, req.headers(), req.uri()).await;
            let failed = matches!(resolution, TokenResolution::Failed(_));
            if !failed {
                req.extensions_mut().insert(resolution.clone());
            }
            let ip = super::client_ip::client_ip(
                req.headers(),
                req.extensions(),
                state.config.server.trusted_proxy_hops,
            );
            let check = tokio::time::timeout(
                CHECK_TIMEOUT,
                RateLimiter::new(state).check(bucket, &limit, resolution.auth(), &ip),
            )
            .await;
            match check {
                Ok(Ok(Verdict { allowed: true, .. })) => inner.call(req).await,
                Ok(Ok(Verdict {
                    retry_after_seconds,
                    ..
                })) => Ok(too_many(retry_after_seconds)),
                Ok(Err(e)) => {
                    // Limiter outage shouldn't take the API down.
                    tracing::warn!(error = %e, bucket, "rate limiter check failed; allowing");
                    inner.call(req).await
                }
                Err(_) => {
                    tracing::warn!(bucket, "rate limiter check timed out; allowing");
                    inner.call(req).await
                }
            }
        })
    }
}
