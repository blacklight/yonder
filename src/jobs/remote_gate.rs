//! Source-side backpressure for remote-fetch jobs.
//!
//! Three cooperating controls keep broken or hostile remotes from
//! filling the remote queues:
//!
//! - **Enqueue dedupe** — [`crate::jobs::Dispatcher`] claims a Redis key
//!   per `(kind, payload)` on remote queues, so a read path that fires
//!   repeatedly can't stack identical jobs.
//! - **Per-domain enqueue budget** — [`enqueue_allowed`] rate-limits new
//!   remote jobs per target domain so one domain can't dominate. It
//!   counts *attempts*: callers check their cheap dedupe/claim markers
//!   first, but a budget check that runs before a claim still spends
//!   budget when the claim loses the race.
//! - **Circuit breaker** — [`record_failure`]/[`record_success`] track
//!   consecutive *remote fetch* failures per domain (local DB or
//!   validation errors must not count); [`enqueue_allowed`] and the job
//!   handlers skip work while a domain's circuit is open.

use crate::cache::Redis;
use crate::config::JobsConfig;

/// Increment a counter and give it a TTL atomically — a plain
/// `INCR` followed by `EXPIRE` can leave a key without a TTL if the
/// process dies in between, which would block (or trip) the domain
/// forever.
static INCR_WITH_TTL_SCRIPT: &str = r#"
local c = redis.call('INCR', KEYS[1])
if c == 1 then
    redis.call('EXPIRE', KEYS[1], ARGV[1])
end
return c
"#;

/// Run [`INCR_WITH_TTL_SCRIPT`]; `None` on any Redis error (the caller
/// decides the fail-open direction).
async fn incr_with_ttl(redis: &Redis, key: &str, ttl_seconds: u64) -> Option<u64> {
    let mut conn = redis.conn().await.ok()?;
    redis::Script::new(INCR_WITH_TTL_SCRIPT)
        .key(key)
        .arg(ttl_seconds)
        .invoke_async::<u64>(&mut *conn)
        .await
        .ok()
}

fn enqueue_key(redis: &Redis, domain: &str) -> String {
    redis.key(&format!("remote-gate:enqueue:{domain}"))
}

fn fail_key(redis: &Redis, domain: &str) -> String {
    redis.key(&format!("remote-gate:fail:{domain}"))
}

fn open_key(redis: &Redis, domain: &str) -> String {
    redis.key(&format!("remote-gate:open:{domain}"))
}

/// Whether a new remote job of `kind` for `domain` may be enqueued or
/// executed. `domain = None` (unparseable URL) always allows — the
/// job's own fetch will fail fast. When the per-domain budget is `0`
/// the limiter is disabled; an open circuit always denies. Denials are
/// logged at `debug` with the `kind` so budget pressure is visible.
///
/// Redis trouble fails open: the queue's own dedupe still applies, and
/// a cache outage must not wedge federation.
pub async fn enqueue_allowed(
    redis: &Redis,
    jobs: &JobsConfig,
    domain: Option<&str>,
    kind: &str,
) -> bool {
    let Some(domain) = domain else { return true };
    let limit = jobs.remote_enqueue_per_domain_per_minute;
    if limit > 0 {
        let full = enqueue_key(redis, domain);
        match incr_with_ttl(redis, &full, 60).await {
            Some(count) if count > u64::from(limit) => {
                tracing::debug!(
                    domain,
                    kind,
                    count,
                    limit,
                    "remote enqueue budget exhausted"
                );
                return false;
            }
            Some(_) => {}
            // Redis error — allow; see the doc comment.
            None => {}
        }
    }
    if circuit_open(redis, domain).await {
        tracing::debug!(domain, kind, "remote circuit open — skipping");
        return false;
    }
    true
}

/// Whether `domain`'s circuit is currently open. Best-effort: Redis
/// errors read as closed so a cache outage can't wedge federation.
pub async fn circuit_open(redis: &Redis, domain: &str) -> bool {
    let mut conn = match redis.conn().await {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    let exists: bool = redis::cmd("EXISTS")
        .arg(open_key(redis, domain))
        .query_async(&mut *conn)
        .await
        .unwrap_or(false);
    exists
}

/// How long the failure counter lives — comfortably longer than the
/// cooldown so a trickle of failures still accumulates across windows.
fn fail_ttl_seconds(jobs: &JobsConfig) -> u64 {
    jobs.remote_circuit_cooldown_seconds
        .saturating_mul(2)
        .max(60)
}

/// Record a successful remote fetch for `domain` — resets the
/// consecutive-failure counter so the next failure streak starts fresh.
pub async fn record_success(redis: &Redis, domain: &str) {
    let mut conn = match redis.conn().await {
        Ok(conn) => conn,
        Err(_) => return,
    };
    let _: redis::RedisResult<()> = redis::cmd("DEL")
        .arg(fail_key(redis, domain))
        .query_async(&mut *conn)
        .await;
}

/// Record a failed **remote fetch** for `domain`. Returns `true` when
/// this failure tripped the circuit (failures reached
/// `remote_circuit_failure_threshold`). Best-effort like the rest of
/// the gate — Redis errors are swallowed. Local errors (DB, validation)
/// must not reach this function: they say nothing about the remote.
pub async fn record_failure(redis: &Redis, jobs: &JobsConfig, domain: &str) -> bool {
    let threshold = jobs.remote_circuit_failure_threshold;
    if threshold == 0 {
        return false;
    }
    let Some(count) = incr_with_ttl(redis, &fail_key(redis, domain), fail_ttl_seconds(jobs)).await
    else {
        return false;
    };
    if count < u64::from(threshold) {
        return false;
    }
    let mut conn = match redis.conn().await {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    let cooldown = jobs.remote_circuit_cooldown_seconds.max(1);
    let _: redis::RedisResult<()> = redis::cmd("SET")
        .arg(open_key(redis, domain))
        .arg("1")
        .arg("EX")
        .arg(cooldown)
        .query_async(&mut *conn)
        .await;
    true
}
