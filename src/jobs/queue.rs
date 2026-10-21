//! Redis Streams primitives for the job queue.
//!
//! Layout under `{redis.key_prefix}`:
//! - `queue:{name}` — one stream per [`Queue`], one `job` JSON field per
//!   entry;
//! - `queue:{name}:dead` — the dead-letter stream for that queue;
//! - `jobs:delayed` — a ZSET of delayed envelopes (score = run-at ms).

use std::time::SystemTime;

use redis::{streams::*, AsyncCommands};

use crate::{
    cache::Redis,
    config::JobsConfig,
    jobs::{JobEnvelope, Queue},
    Error, Result,
};

/// Consumer-group name shared by every queue.
pub const GROUP: &str = "yonder";

fn stream_key(redis: &Redis, queue: Queue) -> String {
    redis.key(&format!("queue:{}", queue.as_str()))
}

fn dead_key(redis: &Redis, queue: Queue) -> String {
    redis.key(&format!("queue:{}:dead", queue.as_str()))
}

fn delayed_key(redis: &Redis) -> String {
    redis.key("jobs:delayed")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds to wait before attempt `attempt` (1-based after the first
/// failure): `base ^ attempt`, capped at `max`.
pub fn backoff_seconds(attempt: u32, base: u64, max: u64) -> u64 {
    base.saturating_pow(attempt.min(31)).min(max)
}

/// Append `envelope` to `queue`'s stream and trim it approximately.
pub async fn push(redis: &Redis, queue: Queue, envelope: &JobEnvelope) -> Result<()> {
    let mut conn = redis.conn().await?;
    let body = serde_json::to_string(envelope).map_err(|e| Error::Internal(e.to_string()))?;
    let _: String = conn
        .xadd(stream_key(redis, queue), "*", &[("job", body.as_str())])
        .await?;
    Ok(())
}

/// Trim `queue`'s stream to `maxlen` entries (approximate).
pub async fn trim(redis: &Redis, queue: Queue, maxlen: u64) -> Result<()> {
    let mut conn = redis.conn().await?;
    let _: usize = conn
        .xtrim(
            stream_key(redis, queue),
            redis::streams::StreamMaxlen::Approx(maxlen as usize),
        )
        .await?;
    Ok(())
}

/// Park `envelope` in the delayed ZSET until `run_at_ms` (unix ms).
pub async fn push_delayed(redis: &Redis, envelope: &JobEnvelope, run_at_ms: u64) -> Result<()> {
    let mut conn = redis.conn().await?;
    let body = serde_json::to_string(envelope).map_err(|e| Error::Internal(e.to_string()))?;
    let _: () = conn
        .zadd(delayed_key(redis), body, run_at_ms as f64)
        .await?;
    Ok(())
}

/// Ensure the consumer group exists on `queue`'s stream (idempotent:
/// `BUSYGROUP` is not an error).
pub async fn ensure_group(redis: &Redis, queue: Queue) -> Result<()> {
    let mut conn = redis.conn().await?;
    let result: std::result::Result<(), redis::RedisError> = conn
        .xgroup_create_mkstream(stream_key(redis, queue), GROUP, "0")
        .await;
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.code() == Some("BUSYGROUP") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Claim up to `count` new entries from `queue` for `consumer`,
/// blocking up to `block_ms` when nothing is available.
///
/// Returns `(stream_id, envelope)` pairs; malformed entries are
/// acked-and-skipped by the caller.
pub async fn read_batch(
    redis: &Redis,
    queue: Queue,
    consumer: &str,
    count: usize,
    block_ms: usize,
) -> Result<Vec<(String, String)>> {
    let mut conn = redis.conn().await?;
    let opts = StreamReadOptions::default()
        .group(GROUP, consumer)
        .block(block_ms)
        .count(count);
    let reply: StreamReadReply = conn
        .xread_options(&[stream_key(redis, queue)], &[">"], &opts)
        .await?;
    let mut entries = Vec::new();
    for key in reply.keys {
        for entry in key.ids {
            match entry.map.get("job") {
                Some(value) => {
                    let body: String = redis::from_redis_value(value)
                        .map_err(|e| Error::Internal(e.to_string()))?;
                    entries.push((entry.id, body));
                }
                None => entries.push((entry.id, String::new())),
            }
        }
    }
    Ok(entries)
}

/// Acknowledge `stream_id` on `queue`.
pub async fn ack(redis: &Redis, queue: Queue, stream_id: &str) -> Result<()> {
    let mut conn = redis.conn().await?;
    let _: usize = conn
        .xack(stream_key(redis, queue), GROUP, &[stream_id])
        .await?;
    Ok(())
}

/// Record a failed delivery: requeue with backoff when attempts remain,
/// otherwise dead-letter. Always acks the original stream entry.
pub async fn fail(
    redis: &Redis,
    queue: Queue,
    stream_id: &str,
    mut envelope: JobEnvelope,
    error: &str,
    jobs: &JobsConfig,
) -> Result<()> {
    envelope.attempts += 1;
    envelope.last_error = Some(error.to_string());
    if envelope.attempts < envelope.max_attempts {
        let backoff = backoff_seconds(
            envelope.attempts,
            jobs.backoff_base_seconds,
            jobs.backoff_max_seconds,
        );
        push_delayed(redis, &envelope, now_ms() + backoff * 1000).await?;
        tracing::info!(
            job = %envelope.id,
            kind = envelope.kind.as_str(),
            attempt = envelope.attempts,
            retry_in = backoff,
            "job failed, scheduled retry"
        );
    } else {
        let mut conn = redis.conn().await?;
        let body = serde_json::to_string(&envelope).map_err(|e| Error::Internal(e.to_string()))?;
        let _: String = conn
            .xadd(dead_key(redis, queue), "*", &[("job", body.as_str())])
            .await?;
        tracing::warn!(
            job = %envelope.id,
            kind = envelope.kind.as_str(),
            attempts = envelope.attempts,
            "job dead-lettered"
        );
    }
    ack(redis, queue, stream_id).await
}

/// Reclaim pending entries idle for at least `min_idle_ms` from dead
/// workers and return them for re-dispatch.
pub async fn reclaim(
    redis: &Redis,
    queue: Queue,
    consumer: &str,
    min_idle_ms: u64,
    count: usize,
) -> Result<Vec<(String, String)>> {
    let mut conn = redis.conn().await?;
    let opts = StreamAutoClaimOptions::default().count(count);
    let reply: StreamAutoClaimReply = conn
        .xautoclaim_options(
            stream_key(redis, queue),
            GROUP,
            consumer,
            min_idle_ms,
            "0",
            opts,
        )
        .await?;
    let mut entries = Vec::new();
    for entry in reply.claimed {
        if let Some(value) = entry.map.get("job") {
            let body: String =
                redis::from_redis_value(value).map_err(|e| Error::Internal(e.to_string()))?;
            entries.push((entry.id, body));
        }
    }
    Ok(entries)
}

/// Move every due delayed job into its target stream. `ZREM` is the
/// commit gate — concurrent pollers can't double-dispatch the same
/// member.
pub async fn dispatch_due(redis: &Redis, limit: usize) -> Result<usize> {
    let mut conn = redis.conn().await?;
    let due: Vec<String> = conn
        .zrangebyscore_limit(
            delayed_key(redis),
            "-inf",
            now_ms() as f64,
            0,
            limit as isize,
        )
        .await?;
    let mut moved = 0;
    for member in due {
        let removed: usize = conn.zrem(delayed_key(redis), &member).await?;
        if removed == 0 {
            continue; // another poller won the member
        }
        match serde_json::from_str::<JobEnvelope>(&member) {
            Ok(envelope) => {
                push(redis, envelope.queue, &envelope).await?;
                moved += 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed delayed job");
            }
        }
    }
    Ok(moved)
}

/// Decode an envelope body; `Err` when malformed.
pub fn decode(body: &str) -> Result<JobEnvelope> {
    serde_json::from_str(body).map_err(|e| Error::Internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_seconds(1, 2, 3600), 2);
        assert_eq!(backoff_seconds(2, 2, 3600), 4);
        assert_eq!(backoff_seconds(3, 2, 3600), 8);
        assert_eq!(backoff_seconds(20, 2, 3600), 3600);
        assert_eq!(backoff_seconds(0, 2, 3600), 1);
    }
}
