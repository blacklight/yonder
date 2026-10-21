//! Redis connection pool and cache layer.
//!
//! [`Redis`] is the shared handle: an async `bb8` pool for Yonder's own
//! operations, plus the raw `redis::Client` epic 06 needs to construct
//! Pubby's sync `RedisDocumentStore`. [`RedisCache`] is the namespaced
//! cache API — get/set with TTL, pattern deletion, and single-flight
//! `get_or_load` request coalescing.
//!
//! Every key Yonder writes is prefixed with `redis.key_prefix`
//! (`yonder:` by default): `{prefix}:cache:` for cached values and
//! `{prefix}:queue:`/`{prefix}:jobs:` for the job queue (see
//! [`crate::jobs`]).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bb8::Pool;
use bb8_redis::RedisConnectionManager;
use redis::{AsyncCommands, Client};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{config::RedisConfig, Error, Result};

/// Async connection pool type used throughout the crate.
pub type RedisPool = Pool<RedisConnectionManager>;

/// Shared Redis handle: async pool + sync client + key namespace.
#[derive(Clone)]
pub struct Redis {
    pool: RedisPool,
    client: Client,
    prefix: String,
}

/// Connect to Redis with the configured pool bound.
///
/// Callers needing guaranteed capacity for long-blocking consumers
/// (the job worker's `XREADGROUP BLOCK` loops) can adjust
/// `config.pool_size` beforehand — see [`crate::jobs::worker`].
pub async fn connect(config: &RedisConfig) -> Result<Redis> {
    let client = Client::open(config.url.as_str())?;
    let manager = RedisConnectionManager::new(config.url.as_str())?;
    let pool = Pool::builder()
        .max_size(config.pool_size)
        // 5s beats bb8's 30s default: a Redis outage should fail fast
        // (and the rate limiter fail-open) rather than stall requests.
        .connection_timeout(Duration::from_secs(5))
        .build(manager)
        .await?;
    Ok(Redis {
        pool,
        client,
        prefix: config.key_prefix.clone(),
    })
}

impl Redis {
    /// Borrow an async connection from the pool.
    pub async fn conn(&self) -> Result<bb8::PooledConnection<'_, RedisConnectionManager>> {
        Ok(self.pool.get().await?)
    }

    /// The raw sync client — epic 06 hands this to Pubby's
    /// `RedisDocumentStore`.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The configured key prefix (no trailing colon).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// A fully namespaced key: `{prefix}:{suffix}` where `suffix` already
    /// carries its domain (`cache:…`, `queue:…`, `jobs:…`).
    pub fn key(&self, suffix: &str) -> String {
        format!("{}:{suffix}", self.prefix)
    }
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Namespaced cache over [`Redis`]: `get`/`set` with TTL,
/// `delete_pattern` via `SCAN`, and single-flight `get_or_load`.
///
/// Values are stored as JSON envelopes `{"v": …, "e": expiry_unix}` —
/// Redis `SET … EX` is the expiry authority; the `e` field keeps entries
/// inspectable from `redis-cli`.
#[derive(Clone)]
pub struct RedisCache {
    redis: Redis,
    inflight: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl RedisCache {
    /// Create a cache over `redis` — keys land under `{prefix}:cache:`.
    pub fn new(redis: Redis) -> Self {
        RedisCache {
            redis,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Full Redis key for a cache entry.
    fn key(&self, key: &str) -> String {
        self.redis.key(&format!("cache:{key}"))
    }

    /// Read a value; `Ok(None)` when absent, expired, or corrupt.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        let mut conn = self.redis.conn().await?;
        let raw: Option<String> = conn.get(self.key(key)).await?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(envelope) => Ok(envelope.get("v").cloned()),
            Err(_) => Ok(None),
        }
    }

    /// Store `value` under `key` for `ttl` (millisecond granularity;
    /// sub-second TTLs are valid).
    pub async fn set(&self, key: &str, value: &impl Serialize, ttl: Duration) -> Result<()> {
        let envelope = json!({
            "v": serde_json::to_value(value).map_err(|e| Error::Internal(e.to_string()))?,
            "e": now_unix() + ttl.as_secs_f64(),
        });
        let mut conn = self.redis.conn().await?;
        let _: () = conn
            .pset_ex(self.key(key), envelope.to_string(), ttl.as_millis() as u64)
            .await?;
        Ok(())
    }

    /// Delete a single key; returns `true` when something was removed.
    pub async fn delete(&self, key: &str) -> Result<bool> {
        let mut conn = self.redis.conn().await?;
        let dropped: usize = conn.del(self.key(key)).await?;
        Ok(dropped > 0)
    }

    /// Delete every cache key matching `pattern` (glob, e.g.
    /// `"user:*"`). Uses `SCAN` — never `KEYS`.
    pub async fn delete_pattern(&self, pattern: &str) -> Result<usize> {
        let full = self.key(pattern);
        let mut conn = self.redis.conn().await?;
        let mut iter = conn.scan_match::<_, String>(full).await?;
        let mut doomed = Vec::new();
        while let Some(key) = iter.next_item().await {
            doomed.push(key);
        }
        drop(iter);
        if doomed.is_empty() {
            return Ok(0);
        }
        let mut dropped = 0;
        // Delete in batches — one giant DEL would block Redis and an
        // unbounded Vec already collected every matching key.
        for chunk in doomed.chunks(500) {
            dropped += conn.del::<_, usize>(chunk).await?;
        }
        Ok(dropped)
    }

    /// Return the cached value for `key`, or call `load()` once.
    ///
    /// Concurrent callers share a single in-flight load: the first
    /// caller renders and stores, waiters block on the same per-key
    /// mutex and then re-read the cache. Coalescing is per process —
    /// other nodes may still load concurrently.
    ///
    /// The per-key mutex is removed from the map once the load
    /// finishes and no other waiter holds it, so the map doesn't grow
    /// with key cardinality.
    pub async fn get_or_load<V, F, Fut>(&self, key: &str, ttl: Duration, load: F) -> Result<V>
    where
        V: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>>,
    {
        if let Some(value) = self.get(key).await? {
            if let Ok(v) = serde_json::from_value(value) {
                return Ok(v);
            }
        }
        let full_key = self.key(key);
        let guard = {
            let mut map = self.inflight.lock().await;
            map.entry(full_key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _lock = guard.lock().await;
        let result: Result<V> = async {
            // Another caller may have filled the cache while we waited.
            let cached = self
                .get(key)
                .await?
                .and_then(|value| serde_json::from_value(value).ok());
            match cached {
                Some(v) => Ok(v),
                None => {
                    let value = load().await?;
                    self.set(key, &value, ttl).await?;
                    Ok(value)
                }
            }
        }
        .await;
        drop(_lock);
        // Drop the map entry only when nobody else is queued on it —
        // a waiter still blocked on `guard` holds a second strong ref.
        let mut map = self.inflight.lock().await;
        if Arc::strong_count(&guard) == 2 {
            map.remove(&full_key);
        }
        result
    }
}
