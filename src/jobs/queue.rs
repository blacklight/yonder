//! Redis Streams primitives for the job queue.
//!
//! Layout under `{redis.key_prefix}`:
//! - `queue:{name}` — one stream per [`Queue`], one `job` JSON field per
//!   entry. Entries are `XACK`ed **and `XDEL`eted** once handled, so the
//!   stream only ever holds outstanding work — it is never trimmed by
//!   length, which would drop undelivered jobs;
//! - `queue:{name}:dead` — the dead-letter stream for that queue,
//!   trimmed approximately to `jobs.stream_maxlen`;
//! - `jobs:delayed` — a ZSET of delayed envelopes (score = run-at ms).

use std::time::SystemTime;

use redis::{streams::*, AsyncCommands, Script};

use crate::{
    cache::Redis,
    config::JobsConfig,
    jobs::{JobEnvelope, Queue},
    Error, Result,
};

/// Consumer-group name shared by every queue.
pub const GROUP: &str = "yonder";

/// Atomically claim and promote one delayed job: `ZREM` is the commit
/// gate for concurrent pollers; the `XADD` runs in the same script so a
/// member can't be removed without being re-queued.
static PROMOTE_SCRIPT: &str = r#"
local removed = redis.call('ZREM', KEYS[1], ARGV[1])
if removed == 0 then
    return 0
end
redis.call('XADD', KEYS[2], '*', 'job', ARGV[1])
return 1
"#;

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

/// Append `envelope` to `queue`'s stream.
pub async fn push(redis: &Redis, queue: Queue, envelope: &JobEnvelope) -> Result<()> {
    let mut conn = redis.conn().await?;
    let body = serde_json::to_string(envelope).map_err(|e| Error::Internal(e.to_string()))?;
    let _: String = conn
        .xadd(stream_key(redis, queue), "*", &[("job", body.as_str())])
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

/// Acknowledge `stream_id` on `queue` and delete the stream entry — the
/// stream then only retains unprocessed work, so it stays bounded
/// without `MAXLEN` trimming (which would drop undelivered jobs).
pub async fn ack(redis: &Redis, queue: Queue, stream_id: &str) -> Result<()> {
    let mut conn = redis.conn().await?;
    redis::pipe()
        .xack(stream_key(redis, queue), GROUP, &[stream_id])
        .xdel(stream_key(redis, queue), &[stream_id])
        .query_async::<()>(&mut *conn)
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
        redis::pipe()
            .xadd(dead_key(redis, queue), "*", &[("job", body.as_str())])
            .xtrim(
                dead_key(redis, queue),
                redis::streams::StreamMaxlen::Approx(jobs.stream_maxlen as usize),
            )
            .query_async::<()>(&mut *conn)
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
/// workers and return them for re-dispatch. Entries without a `job`
/// field are returned as empty bodies so the caller acks them out of
/// the pending list.
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
        let body = match entry.map.get("job") {
            Some(value) => {
                redis::from_redis_value(value).map_err(|e| Error::Internal(e.to_string()))?
            }
            None => String::new(),
        };
        entries.push((entry.id, body));
    }
    Ok(entries)
}

/// Move every due delayed job into its target stream. Promotion is
/// atomic per member (`ZREM` + `XADD` in one script): concurrent
/// pollers can't double-dispatch, and a job can't vanish between the
/// two commands. A failed member is logged and the rest of the batch
/// proceeds.
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
        let queue = match serde_json::from_str::<JobEnvelope>(&member) {
            Ok(envelope) => envelope.queue,
            Err(e) => {
                tracing::warn!(error = %e, "dropping malformed delayed job");
                let _: usize = conn.zrem(delayed_key(redis), &member).await?;
                continue;
            }
        };
        let promoted: std::result::Result<u32, redis::RedisError> = Script::new(PROMOTE_SCRIPT)
            .key(delayed_key(redis))
            .key(stream_key(redis, queue))
            .arg(&member)
            .invoke_async(&mut *conn)
            .await;
        match promoted {
            Ok(1) => moved += 1,
            Ok(_) => {} // another poller won the member
            Err(e) => {
                tracing::warn!(error = %e, "delayed-job promotion failed; will retry")
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
