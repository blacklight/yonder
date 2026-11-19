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
    // Every queue consumer holds a pooled connection across its
    // blocking read — 9 lanes plus the delayed/reclaim/depth loops
    // need more than the default pool of 10.
    let mut config = RedisConfig {
        pool_size: 64,
        ..Default::default()
    };
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
        handler_timeout_seconds: 20,
        reclaim_idle_seconds: 90,
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

/// Remote-queue enqueues dedupe on `(kind, payload)`: a repeat trigger
/// returns the existing job id instead of stacking a second stream
/// entry, while a different payload still enqueues.
#[tokio::test]
async fn remote_enqueue_dedupes_identical_payloads() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    let payload = json!({"actor_url": "https://example.com/@alice"});

    let first = dispatcher
        .enqueue(JobKind::RemoteActorFetch, payload.clone())
        .await
        .unwrap();
    let second = dispatcher
        .enqueue(JobKind::RemoteActorFetch, payload)
        .await
        .unwrap();
    assert_eq!(
        first, second,
        "duplicate remote enqueue must reuse the job id"
    );

    let other = dispatcher
        .enqueue(
            JobKind::RemoteActorFetch,
            json!({"actor_url": "https://example.com/@bob"}),
        )
        .await
        .unwrap();
    assert_ne!(first, other);

    // The dedupe claim lives until the job settles: read the two
    // entries into the pending list under a throwaway consumer,
    // dead-letter the first (which releases its claim — the worker's
    // ack path calls the same `release_dedupe`), and confirm a fresh
    // enqueue of that payload is no longer deduped.
    queue::ensure_group(&redis, Queue::Remote).await.unwrap();
    let mut conn = redis.conn().await.unwrap();
    let stream = redis.key(&format!("queue:{}", Queue::Remote.as_str()));
    let reply: redis::streams::StreamReadReply = redis::AsyncCommands::xread_options(
        &mut *conn,
        &[stream.as_str()],
        &[">"],
        &redis::streams::StreamReadOptions::default()
            .group(queue::GROUP, "dedupe-test")
            .count(10),
    )
    .await
    .unwrap();
    let entries: Vec<(String, JobEnvelope)> = reply
        .keys
        .into_iter()
        .flat_map(|k| k.ids)
        .filter_map(|i| {
            let body: String = redis::from_redis_value(i.map.get("job")?).ok()?;
            let env: JobEnvelope = queue::decode(&body).ok()?;
            Some((i.id, env))
        })
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "expected exactly two distinct remote jobs"
    );
    let (stream_id, mut envelope) = entries
        .into_iter()
        .find(|(_, env)| env.id == first)
        .expect("first job entry present in stream");
    assert!(
        envelope.dedupe_key.is_some(),
        "remote job must carry a dedupe claim"
    );
    drop(conn);

    envelope.attempts = envelope.max_attempts;
    queue::fail(
        &redis,
        Queue::Remote,
        &stream_id,
        envelope,
        "test dead-letter",
        &jobs_config,
    )
    .await
    .unwrap();

    let third = dispatcher
        .enqueue(
            JobKind::RemoteActorFetch,
            json!({"actor_url": "https://example.com/@alice"}),
        )
        .await
        .unwrap();
    assert_ne!(
        first, third,
        "settling the job must release the dedupe claim so work can be re-enqueued"
    );
}

/// A job sitting on a stale lane (enqueued before its kind's routing
/// changed, or promoted from a pre-refactor delayed retry) must be
/// moved to `kind.queue()` by the worker instead of executing on the
/// wrong queue — the remote lane then runs it under its own permits.
#[tokio::test]
async fn stale_lane_job_reroutes_to_kind_queue() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(JobKind::RemoteActorFetch, CountHandler(counter.clone()));

    let jobs_config = test_jobs();
    let dispatcher = Dispatcher::new(&redis, &jobs_config);
    // `enqueue_on` bypasses `kind.queue()` — exactly the stale-lane
    // shape left behind by a routing change.
    dispatcher
        .enqueue_on(
            Queue::Default,
            JobKind::RemoteActorFetch,
            json!({"actor_url": "https://example.com/@carol"}),
        )
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
        "rerouted job was not executed"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();

    // Both streams drained: the stale entry acked out of `default`,
    // the rerouted copy consumed from `remote`.
    assert_eq!(pending_count(&redis, Queue::Default).await, 0);
    assert_eq!(pending_count(&redis, Queue::Remote).await, 0);
    let mut conn = redis.conn().await.unwrap();
    for name in [Queue::Default, Queue::Remote] {
        let len: u64 =
            redis::AsyncCommands::xlen(&mut *conn, redis.key(&format!("queue:{}", name.as_str())))
                .await
                .unwrap();
        assert_eq!(len, 0, "{} stream should be empty", name.as_str());
    }
}

/// The per-domain enqueue budget allows up to the configured count
/// per minute and then denies; other domains and unparseable URLs are
/// unaffected.
#[tokio::test]
async fn remote_gate_budget_limits_per_domain() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let jobs = JobsConfig {
        remote_enqueue_per_domain_per_minute: 2,
        ..Default::default()
    };
    let domain = Some("budget-test.example");
    let bulk = yonder::jobs::remote_gate::GateClass::Bulk;
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await
    );
    assert!(
        !yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await,
        "third enqueue within the minute must be denied"
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(
            &redis,
            &jobs,
            Some("other-budget-test.example"),
            bulk,
            "actor_fetch"
        )
        .await
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, None, bulk, "actor_fetch").await
    );
}

/// The budget and circuit are namespaced by [`GateClass`]: an open
/// bulk circuit (or an exhausted bulk budget) must not deny
/// user-visible status fetches for the same domain, and status-fetch
/// failures must not trip the bulk circuit.
#[tokio::test]
async fn remote_gate_classes_are_isolated() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let jobs = JobsConfig {
        remote_enqueue_per_domain_per_minute: 1,
        remote_circuit_failure_threshold: 1,
        remote_circuit_cooldown_seconds: 60,
        ..Default::default()
    };
    let domain = Some("gate-class-test.example");
    let bulk = yonder::jobs::remote_gate::GateClass::Bulk;
    let status = yonder::jobs::remote_gate::GateClass::Status;

    // Trip the bulk circuit — the status class must stay open.
    assert!(
        yonder::jobs::remote_gate::record_failure(&redis, &jobs, "gate-class-test.example", bulk)
            .await
    );
    assert!(yonder::jobs::remote_gate::circuit_open(&redis, "gate-class-test.example", bulk).await);
    assert!(
        !yonder::jobs::remote_gate::circuit_open(&redis, "gate-class-test.example", status).await,
        "bulk failures must not open the status circuit"
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, status, "status_fetch")
            .await,
        "status fetches must not be denied by an open bulk circuit"
    );
    assert!(
        !yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await,
        "the bulk circuit still denies bulk enqueues"
    );

    // Exhaust the bulk budget — the status budget is separate. A
    // fresh domain keeps the per-minute counters from part one out of
    // the way.
    let domain = Some("gate-class-budget-test.example");
    let jobs = JobsConfig {
        remote_enqueue_per_domain_per_minute: 1,
        remote_circuit_failure_threshold: 0,
        ..Default::default()
    };
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await
    );
    assert!(
        !yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, bulk, "actor_fetch")
            .await,
        "second bulk enqueue within the minute must be denied"
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(&redis, &jobs, domain, status, "status_fetch")
            .await,
        "bulk budget exhaustion must not deny status fetches"
    );

    // Status-fetch failures trip only the status circuit — again on
    // a fresh domain, since part one already opened the bulk circuit
    // for `gate-class-test.example`.
    let jobs = JobsConfig {
        remote_circuit_failure_threshold: 1,
        remote_circuit_cooldown_seconds: 60,
        ..Default::default()
    };
    assert!(
        yonder::jobs::remote_gate::record_failure(
            &redis,
            &jobs,
            "gate-class-status-test.example",
            status
        )
        .await
    );
    assert!(
        yonder::jobs::remote_gate::circuit_open(&redis, "gate-class-status-test.example", status)
            .await
    );
    assert!(
        !yonder::jobs::remote_gate::circuit_open(&redis, "gate-class-status-test.example", bulk)
            .await,
        "status failures must not open the bulk circuit"
    );
}

/// The circuit breaker opens after the configured number of
/// consecutive fetch failures, blocks enqueues meanwhile, and a
/// success resets the streak.
#[tokio::test]
async fn remote_gate_circuit_trips_and_resets() {
    let _permit = harness::acquire().await;
    let Some(redis) = test_redis().await else {
        return;
    };
    let jobs = JobsConfig {
        remote_circuit_failure_threshold: 2,
        remote_circuit_cooldown_seconds: 60,
        ..Default::default()
    };
    let domain = "circuit-test.example";
    let bulk = yonder::jobs::remote_gate::GateClass::Bulk;
    assert!(!yonder::jobs::remote_gate::circuit_open(&redis, domain, bulk).await);
    assert!(!yonder::jobs::remote_gate::record_failure(&redis, &jobs, domain, bulk).await);
    assert!(!yonder::jobs::remote_gate::circuit_open(&redis, domain, bulk).await);
    assert!(yonder::jobs::remote_gate::record_failure(&redis, &jobs, domain, bulk).await);
    assert!(yonder::jobs::remote_gate::circuit_open(&redis, domain, bulk).await);
    assert!(
        !yonder::jobs::remote_gate::enqueue_allowed(
            &redis,
            &jobs,
            Some(domain),
            bulk,
            "actor_fetch"
        )
        .await,
        "open circuit must deny enqueues"
    );
    assert!(
        yonder::jobs::remote_gate::enqueue_allowed(
            &redis,
            &jobs,
            Some("other-circuit-test.example"),
            bulk,
            "actor_fetch"
        )
        .await
    );

    // A success resets the streak: after one failure + one success,
    // a single further failure must not reach the threshold of two.
    let domain2 = "circuit-reset-test.example";
    assert!(!yonder::jobs::remote_gate::record_failure(&redis, &jobs, domain2, bulk).await);
    yonder::jobs::remote_gate::record_success(&redis, domain2, bulk).await;
    assert!(!yonder::jobs::remote_gate::record_failure(&redis, &jobs, domain2, bulk).await);
    assert!(
        !yonder::jobs::remote_gate::circuit_open(&redis, domain2, bulk).await,
        "a success must reset the consecutive-failure streak"
    );
}

/// Deleting a queue's stream destroys its consumer group — the state
/// a Redis restart without persistence, a `maxmemory` eviction or a
/// flush leaves behind. The worker must recreate it on its own;
/// before the `NOGROUP` self-heal every consumer loop warned-and-
/// retried forever and jobs piled up until restart.
#[tokio::test]
async fn stream_loss_self_heals() {
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
        .enqueue(JobKind::EmailSend, json!({}))
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
        "first job was not executed"
    );

    // Wipe the stream — consumer group gone with it.
    {
        let mut conn = redis.conn().await.unwrap();
        let dropped: usize = redis::AsyncCommands::del(
            &mut *conn,
            redis.key(&format!("queue:{}", Queue::Default.as_str())),
        )
        .await
        .unwrap();
        assert_eq!(dropped, 1, "stream was expected to exist");
    }

    // Jobs enqueued after the loss must still flow: the reader heals
    // the group and the recreated stream replays them.
    dispatcher
        .enqueue(JobKind::EmailSend, json!({}))
        .await
        .unwrap();
    let counter_ref = counter.clone();
    assert!(
        wait_for(Duration::from_secs(10), move || {
            let counter_ref = counter_ref.clone();
            async move { counter_ref.load(Ordering::SeqCst) >= 2 }
        })
        .await,
        "job enqueued after stream loss was not executed"
    );
    cancel.cancel();
    handle.await.unwrap().unwrap();
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
