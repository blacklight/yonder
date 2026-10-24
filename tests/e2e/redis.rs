//! Redis integration tests: cache layer, job queue (claim/ack/retry/
//! dead-letter/delayed), worker and scheduler, run against a real Redis
//! in a testcontainer.
//!
//! Skipped (with a message, never silently green) when no
//! Docker-compatible container runtime is reachable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tokio_cron_scheduler::JobScheduler;
use yonder::cache::{Redis, RedisCache};
use yonder::config::{Config, JobsConfig, RedisConfig};
use yonder::jobs::{
    self, queue, Dispatcher, JobContext, JobEnvelope, JobHandler, JobKind, Queue, Registry,
};
use yonder::Error;

use crate::harness;

/// Per-test namespace on the shared Redis — see `harness`.
async fn test_redis() -> Option<Redis> {
    let infra = harness::infra().await?;
    let mut config = RedisConfig::default();
    Some(harness::scoped_redis(infra, &mut config, &harness::test_slug()).await)
}

/// JobContext with a lazy (never-connected) Postgres pool — stub
/// handlers never touch the DB.
fn test_ctx(redis: &Redis) -> JobContext {
    let db = PgPoolOptions::new()
        .connect_lazy("postgres://yonder:yonder@127.0.0.1:1/yonder")
        .expect("lazy pool");
    JobContext {
        db,
        redis: redis.clone(),
        config: Arc::new(Config::default()),
        media: yonder::media::storage::MediaStore::from_config(&Config::default()),
    }
}

/// Fast worker settings for tests.
fn test_jobs() -> Arc<JobsConfig> {
    Arc::new(JobsConfig {
        concurrency: 1,
        poll_interval_ms: 50,
        reclaim_idle_seconds: 30,
        ..Default::default()
    })
}

struct CountHandler(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl JobHandler for CountHandler {
    async fn run(&self, _ctx: &JobContext, _payload: &Value) -> yonder::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FailHandler(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl JobHandler for FailHandler {
    async fn run(&self, _ctx: &JobContext, _payload: &Value) -> yonder::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::Internal("boom".to_string()))
    }
}

/// Poll `check` until it returns true or `timeout` elapses.
async fn wait_for<F, Fut>(timeout: Duration, check: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Pending-entry count for the default queue's consumer group.
async fn pending_count(redis: &Redis, queue_name: Queue) -> usize {
    let mut conn = redis.conn().await.unwrap();
    let reply: redis::streams::StreamPendingReply = redis::AsyncCommands::xpending(
        &mut *conn,
        redis.key(&format!("queue:{}", queue_name.as_str())),
        queue::GROUP,
    )
    .await
    .unwrap();
    reply.count()
}

async fn dead_entries(redis: &Redis, queue_name: Queue) -> Vec<JobEnvelope> {
    let mut conn = redis.conn().await.unwrap();
    let reply: redis::streams::StreamRangeReply = redis::AsyncCommands::xrange_all(
        &mut *conn,
        redis.key(&format!("queue:{}:dead", queue_name.as_str())),
    )
    .await
    .unwrap();
    reply
        .ids
        .iter()
        .filter_map(|e| e.map.get("job"))
        .filter_map(|v| redis::from_redis_value::<String>(v).ok())
        .filter_map(|body| queue::decode(&body).ok())
        .collect()
}

fn worker_for(
    ctx: &JobContext,
    registry: Registry,
    jobs_config: Arc<JobsConfig>,
) -> jobs::worker::Worker {
    jobs::worker::Worker::new(
        ctx.clone(),
        Arc::new(registry),
        jobs_config,
        "test-consumer",
    )
}

#[tokio::test]
async fn cache_round_trip() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let cache = RedisCache::new(redis);

    cache
        .set("greeting", &json!("hello"), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(cache.get("greeting").await.unwrap(), Some(json!("hello")));
    assert_eq!(cache.get("missing").await.unwrap(), None);

    // TTL expiry — 1s is the minimum Redis `EX` resolution.
    cache
        .set("ephemeral", &json!(1), Duration::from_secs(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(cache.get("ephemeral").await.unwrap(), None);

    // Pattern deletion.
    for i in 0..3 {
        cache
            .set(&format!("user:{i}"), &json!(i), Duration::from_secs(60))
            .await
            .unwrap();
    }
    assert_eq!(cache.delete_pattern("user:*").await.unwrap(), 3);
    assert_eq!(cache.get("user:0").await.unwrap(), None);
}

#[tokio::test]
async fn cache_single_flight() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let cache = RedisCache::new(redis);
    let renders = Arc::new(AtomicUsize::new(0));

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let cache = cache.clone();
        let renders = renders.clone();
        set.spawn(async move {
            cache
                .get_or_load::<i64, _, _>("shared", Duration::from_secs(60), move || {
                    let renders = renders.clone();
                    async move {
                        renders.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Ok(42)
                    }
                })
                .await
        });
    }
    while let Some(result) = set.join_next().await {
        assert_eq!(result.unwrap().unwrap(), 42);
    }
    assert_eq!(renders.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn enqueue_execute_ack() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::EmailSend, CountHandler(counter.clone()));

    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    dispatcher
        .enqueue(JobKind::EmailSend, json!({"to": "user@example.com"}))
        .await
        .unwrap();

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(10), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 1 }
        })
        .await,
        "job was not executed"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();

    // Awaiting shutdown before checking the PEL keeps this deterministic.
    assert_eq!(pending_count(&redis, Queue::Default).await, 0);
}

#[tokio::test]
async fn failing_job_retries_then_dead_letters() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::EmailSend, FailHandler(counter.clone()));

    let mut jobs_config = (*test_jobs()).clone();
    jobs_config.max_attempts = 2;
    jobs_config.backoff_base_seconds = 1;
    let jobs_config = Arc::new(jobs_config);

    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    dispatcher
        .enqueue(JobKind::EmailSend, json!({}))
        .await
        .unwrap();

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    let redis_ref = redis.clone();
    assert!(
        wait_for(Duration::from_secs(15), move || {
            let redis_ref = redis_ref.clone();
            async move { !dead_entries(&redis_ref, Queue::Default).await.is_empty() }
        })
        .await,
        "job was never dead-lettered"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();

    // Two delivery attempts total, then the DLQ entry carries the error.
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    let dead = dead_entries(&redis, Queue::Default).await;
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].attempts, 2);
    assert!(dead[0].last_error.as_deref().unwrap().contains("boom"));
    assert_eq!(pending_count(&redis, Queue::Default).await, 0);
}

#[tokio::test]
async fn delayed_job_fires() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::RemoteActorFetch, CountHandler(counter.clone()));

    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    let start = Instant::now();
    dispatcher
        .enqueue_delayed(
            JobKind::RemoteActorFetch,
            json!({}),
            Duration::from_millis(800),
        )
        .await
        .unwrap();

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    // Too early — the job is still parked in the delayed ZSET.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(10), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 1 }
        })
        .await,
        "delayed job never fired"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert!(start.elapsed() >= Duration::from_millis(800));
}

#[tokio::test]
async fn cron_job_fires() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::CleanupRemoteMedia, CountHandler(counter.clone()));

    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    let mut config = Config::default();
    config.media.remote_cleanup_schedule = Some("* * * * * *".to_string());
    let mut scheduler: JobScheduler = jobs::scheduler::start(&config, &dispatcher).await.unwrap();

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(15), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 1 }
        })
        .await,
        "cron job never fired"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
    scheduler.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_survives() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));

    // Enqueue with no worker running — the job persists in the stream.
    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    dispatcher
        .enqueue(JobKind::NotificationFanout, json!({}))
        .await
        .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    // "Restart": a fresh worker picks the job up.
    let mut registry = Registry::default();
    registry.register(JobKind::NotificationFanout, CountHandler(counter.clone()));
    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(10), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 1 }
        })
        .await,
        "queued job was lost"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
}

struct PanicOnceHandler(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl JobHandler for PanicOnceHandler {
    async fn run(&self, _ctx: &JobContext, _payload: &Value) -> yonder::Result<()> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("first attempt panics");
        }
        Ok(())
    }
}

#[tokio::test]
async fn backlog_beyond_maxlen_is_not_dropped() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::EmailSend, CountHandler(counter.clone()));

    let mut jobs_config = (*test_jobs()).clone();
    jobs_config.stream_maxlen = 5;
    let jobs_config = Arc::new(jobs_config);

    // Enqueue far more than stream_maxlen with no worker running —
    // live queues are never length-trimmed, so every job must run.
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    const JOBS: usize = 20;
    for _ in 0..JOBS {
        dispatcher
            .enqueue(JobKind::EmailSend, json!({}))
            .await
            .unwrap();
    }

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(15), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= JOBS }
        })
        .await,
        "only {} of {JOBS} queued jobs executed",
        counter.load(Ordering::SeqCst)
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert_eq!(pending_count(&redis, Queue::Default).await, 0);
}

#[tokio::test]
async fn panicking_handler_fails_the_job_and_the_worker_survives() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::EmailSend, PanicOnceHandler(counter.clone()));

    let mut jobs_config = (*test_jobs()).clone();
    jobs_config.max_attempts = 2;
    jobs_config.backoff_base_seconds = 1;
    let jobs_config = Arc::new(jobs_config);

    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    dispatcher
        .enqueue(JobKind::EmailSend, json!({}))
        .await
        .unwrap();

    let worker = worker_for(&test_ctx(&redis), registry, jobs_config);
    let cancel = worker.cancel_token();
    let handle = tokio::spawn(async move { worker.run().await });

    // First attempt panics; the retry (1s backoff) succeeds — the
    // consumer task must survive the panic.
    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(15), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 2 }
        })
        .await,
        "panicking job was not retried"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
    assert_eq!(pending_count(&redis, Queue::Default).await, 0);
    assert!(dead_entries(&redis, Queue::Default).await.is_empty());
}
