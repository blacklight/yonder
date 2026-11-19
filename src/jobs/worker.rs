//! The worker: claims jobs from every queue, dispatches them to the
//! registered handlers, acks on success, and retries with exponential
//! backoff before dead-lettering. A delayed-job poller and an
//! `XAUTOCLAIM` reclaimer run alongside it.
//!
//! Run it in-process via `jobs.enabled` on `serve`, or standalone via
//! `yonder worker`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
    config::JobsConfig,
    jobs::{queue, JobContext, JobEnvelope, Queue, Registry},
    Result,
};

/// Slack kept between `handler_timeout_seconds` and
/// `reclaim_idle_seconds` so a permit wait plus a full handler run
/// never lets `XAUTOCLAIM` re-dispatch a still-running remote job.
pub const RECLAIM_SAFETY_MARGIN_MS: u64 = 60_000;

/// Outcome of waiting for a remote permit on an already-claimed job.
enum PermitWait {
    /// A slot was acquired; run the job holding it.
    Acquired(tokio::sync::OwnedSemaphorePermit),
    /// No cap applies (non-remote queue, cap disabled, semaphore
    /// closed) — run without a permit.
    NotRequired,
    /// No slot within the safe window — leave the job pending.
    TimedOut,
}

/// A worker draining every [`Queue`] plus the delayed-job ZSET.
#[derive(Clone)]
pub struct Worker {
    ctx: JobContext,
    registry: Arc<Registry>,
    jobs: Arc<JobsConfig>,
    consumer: String,
    cancel: CancellationToken,
    /// Global cap on concurrently executing remote-queue jobs
    /// (`jobs.remote_max_in_flight`; `None` = unlimited).
    remote_permits: Option<Arc<Semaphore>>,
}

impl Worker {
    /// Create a worker; `consumer` names this instance in the consumer
    /// group (`hostname:pid` in production, any string in tests).
    pub fn new(
        ctx: JobContext,
        registry: Arc<Registry>,
        jobs: Arc<JobsConfig>,
        consumer: impl Into<String>,
    ) -> Self {
        let remote_permits = jobs
            .remote_max_in_flight
            .filter(|n| *n > 0)
            .map(|n| Arc::new(Semaphore::new(n as usize)));
        Worker {
            ctx,
            registry,
            jobs,
            consumer: consumer.into(),
            cancel: CancellationToken::new(),
            remote_permits,
        }
    }

    /// Token that stops the worker when cancelled.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run until [`Self::cancel_token`] fires: consumer tasks per queue
    /// (up to `jobs.concurrency`), the delayed-job poller and the
    /// pending-entry reclaimer.
    pub async fn run(&self) -> Result<()> {
        for queue_name in Queue::ALL {
            queue::ensure_group(&self.ctx.redis, *queue_name).await?;
        }
        let mut set = JoinSet::new();
        for queue_name in Queue::ALL {
            for _ in 0..queue_name.consumer_count(self.jobs.as_ref()) {
                let worker = self.clone();
                set.spawn(async move { worker.consumer_loop(*queue_name).await });
            }
        }
        let delayed = self.clone();
        set.spawn(async move { delayed.delayed_loop().await });
        let reclaimer = self.clone();
        set.spawn(async move { reclaimer.reclaim_loop().await });
        let depths = self.clone();
        set.spawn(async move { depths.depth_loop().await });
        self.cancel.cancelled().await;
        // Drain instead of aborting: each task observes the token at
        // its loop boundary, so a dispatch in flight runs to
        // completion and acks. Aborting could cut a job between
        // handler completion and `XACK`, orphaning the entry in the
        // pending list until the reclaimer re-runs it.
        while let Some(result) = set.join_next().await {
            if let Err(e) = result {
                tracing::warn!(error = %e, "worker task terminated with error");
            }
        }
        Ok(())
    }

    /// Remote-queue permit for `queue_name`, if the global cap is
    /// configured. Acquired **before** a job is claimed from the
    /// stream: a job sitting in the consumer's pending list while it
    /// waits for a permit would look abandoned to `XAUTOCLAIM` and be
    /// re-dispatched while still waiting.
    async fn acquire_remote_permit(
        &self,
        queue_name: Queue,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if !queue_name.is_remote() {
            return None;
        }
        let sem = self.remote_permits.as_ref()?;
        tokio::select! {
            _ = self.cancel.cancelled() => None,
            permit = sem.clone().acquire_owned() => permit.ok(),
        }
    }

    /// Permit for a job the reclaimer already pulled into the pending
    /// list: its idle clock started at the reclaim, so the wait is
    /// bounded well inside `reclaim_idle_seconds` and the pending idle
    /// time is refreshed (via `XPENDING IDLE`) while waiting. On
    /// timeout the job is left pending for the next reclaim pass
    /// rather than dispatched without a slot.
    async fn wait_remote_permit(
        &self,
        queue_name: Queue,
        stream_id: &str,
        reclaim_idle_ms: u64,
    ) -> PermitWait {
        let Some(sem) = self.remote_permits.as_ref() else {
            return PermitWait::NotRequired;
        };
        if !queue_name.is_remote() {
            return PermitWait::NotRequired;
        }
        // Budget: handler timeout + this wait + slack must stay below
        // the reclaim window (validated in `Config::validate`). The
        // wait is also capped at half the idle window so a fresh
        // reclaim always gets at least that long.
        let budget = self
            .jobs
            .reclaim_idle_seconds
            .saturating_mul(1000)
            .saturating_sub(self.jobs.handler_timeout_seconds.saturating_mul(1000))
            .saturating_sub(RECLAIM_SAFETY_MARGIN_MS)
            .max(1_000)
            .min(reclaim_idle_ms / 2);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!(
                    queue = queue_name.as_str(),
                    "remote permit wait timed out; leaving job pending"
                );
                return PermitWait::TimedOut;
            }
            let step = remaining.min(Duration::from_secs(10));
            tokio::select! {
                _ = self.cancel.cancelled() => return PermitWait::TimedOut,
                permit = tokio::time::timeout(step, sem.clone().acquire_owned()) => {
                    match permit {
                        Ok(Ok(p)) => return PermitWait::Acquired(p),
                        Ok(Err(_)) => return PermitWait::NotRequired, // semaphore closed
                        Err(_) => {}
                    }
                }
            }
            self.refresh_pending_idle(queue_name, stream_id).await;
        }
    }

    /// Reset the pending idle clock for one entry — best-effort; a
    /// failure just means the reclaimer may pick the entry up again
    /// sooner.
    async fn refresh_pending_idle(&self, queue_name: Queue, stream_id: &str) {
        let mut conn = match self.ctx.redis.conn().await {
            Ok(conn) => conn,
            Err(_) => return,
        };
        let _: redis::RedisResult<()> = redis::cmd("XPENDING")
            .arg(queue::stream_key(&self.ctx.redis, queue_name))
            .arg(queue::GROUP)
            .arg(self.consumer.as_str())
            .arg("IDLE")
            .arg(0u64)
            .arg(stream_id)
            .arg(stream_id)
            .query_async(&mut *conn)
            .await;
    }

    /// `XREADGROUP BLOCK` loop for a single queue.
    async fn consumer_loop(&self, queue_name: Queue) {
        let block_ms = self.jobs.poll_interval_ms as usize;
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            match queue::read_batch(&self.ctx.redis, queue_name, &self.consumer, 10, block_ms).await
            {
                Ok(entries) => {
                    for (stream_id, body) in entries {
                        if self.cancel.is_cancelled() {
                            return;
                        }
                        // One permit per job, and only after the claim —
                        // waiting here keeps the entry idle for at most
                        // one dispatch, far below `reclaim_idle_seconds`.
                        let permit = self.acquire_remote_permit(queue_name).await;
                        if self.cancel.is_cancelled() {
                            return;
                        }
                        self.dispatch(queue_name, &stream_id, &body, permit).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(queue = queue_name.as_str(), error = %e, "queue read failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Execute one claimed entry: decode, find the handler, run, ack or
    /// fail with retry/dead-letter semantics. `permit` is the remote
    /// slot already held by the caller (see [`Self::consumer_loop`]).
    async fn dispatch(
        &self,
        queue_name: Queue,
        stream_id: &str,
        body: &str,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        let _remote_permit = permit;
        let envelope = match queue::decode(body) {
            Ok(envelope) => envelope,
            Err(e) => {
                tracing::warn!(stream_id, error = %e, "acking malformed job entry");
                let _ = queue::ack(&self.ctx.redis, queue_name, stream_id).await;
                return;
            }
        };
        // Stale-lane reroute: entries enqueued before a kind's routing
        // changed (or promoted from a delayed retry that predates it)
        // still sit on the old stream. Move them to `kind.queue()` —
        // rewriting `envelope.queue` so subsequent retries promote to
        // the right lane — instead of executing remote work on a
        // latency-sensitive queue without its permits. The dedupe
        // claim rides along in the envelope; pushing raw (not via the
        // Dispatcher) keeps the job id and avoids a second claim.
        let target = envelope.kind.queue();
        if target != queue_name {
            let mut moved = envelope.clone();
            moved.queue = target;
            match queue::push(&self.ctx.redis, target, &moved).await {
                Ok(()) => {
                    tracing::info!(
                        job = %envelope.id,
                        kind = envelope.kind.as_str(),
                        from = queue_name.as_str(),
                        to = target.as_str(),
                        "rerouted stale job to its kind's queue"
                    );
                    if let Err(e) = queue::ack(&self.ctx.redis, queue_name, stream_id).await {
                        tracing::warn!(stream_id, error = %e, "rerouted job ack failed");
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        job = %envelope.id,
                        error = %e,
                        "queue reroute push failed; dispatching in place"
                    );
                }
            }
        }
        let Some(handler) = self.registry.handler(envelope.kind) else {
            tracing::warn!(
                kind = envelope.kind.as_str(),
                "no handler registered — dead-lettering"
            );
            let _ = queue::fail(
                &self.ctx.redis,
                queue_name,
                stream_id,
                JobEnvelope {
                    max_attempts: envelope.attempts.max(1),
                    ..envelope
                },
                "no handler registered",
                &self.jobs,
            )
            .await;
            return;
        };
        // Run the handler in its own task under a timeout: a panic
        // surfaces as a JoinError instead of killing this consumer, and
        // a runaway job is failed instead of holding the consumer slot
        // past `reclaim_idle_seconds` (which would re-dispatch it while
        // still running).
        let timeout = Duration::from_secs(self.jobs.handler_timeout_seconds);
        let mut task = tokio::spawn({
            let handler = handler.clone();
            let ctx = self.ctx.clone();
            let payload = envelope.payload.clone();
            async move { handler.run(&ctx, &payload).await }
        });
        let outcome = match tokio::time::timeout(timeout, &mut task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(crate::Error::Internal(format!("job handler panicked: {e}"))),
            Err(_) => {
                task.abort();
                Err(crate::Error::Internal(format!(
                    "job timed out after {}s",
                    timeout.as_secs()
                )))
            }
        };
        match outcome {
            Ok(()) => {
                if let Err(e) = queue::ack(&self.ctx.redis, queue_name, stream_id).await {
                    tracing::warn!(stream_id, error = %e, "job ack failed");
                } else if let Some(key) = &envelope.dedupe_key {
                    queue::release_dedupe(&self.ctx.redis, key, &envelope.id).await;
                }
            }
            Err(e) => {
                tracing::info!(
                    job = %envelope.id,
                    kind = envelope.kind.as_str(),
                    error = %e,
                    "job execution failed"
                );
                if let Err(e) = queue::fail(
                    &self.ctx.redis,
                    queue_name,
                    stream_id,
                    envelope,
                    &e.to_string(),
                    &self.jobs,
                )
                .await
                {
                    tracing::warn!(stream_id, error = %e, "job failure bookkeeping failed");
                }
            }
        }
    }

    /// Periodic stream-length log so queue starvation is visible before
    /// it becomes a user-visible bug. Non-empty queues only; one line
    /// per minute keeps it cheap to scan.
    async fn depth_loop(&self) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                _ = self.cancel.cancelled() => return,
            }
            for queue_name in Queue::ALL {
                match queue::depth(&self.ctx.redis, *queue_name).await {
                    Ok(len) if len > 0 => {
                        tracing::info!(queue = queue_name.as_str(), depth = len, "queue depth");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(queue = queue_name.as_str(), error = %e, "queue depth read failed");
                    }
                }
            }
        }
    }

    /// Move due delayed jobs into their target streams.
    async fn delayed_loop(&self) {
        let interval = Duration::from_millis(self.jobs.poll_interval_ms.max(50));
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            if let Err(e) = queue::dispatch_due(&self.ctx.redis, 100).await {
                tracing::warn!(error = %e, "delayed-job dispatch failed");
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.cancel.cancelled() => return,
            }
        }
    }

    /// Reclaim pending entries abandoned by dead workers.
    async fn reclaim_loop(&self) {
        let idle_ms = self.jobs.reclaim_idle_seconds * 1000;
        let interval = Duration::from_secs(self.jobs.reclaim_idle_seconds.max(30) / 10);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.cancel.cancelled() => return,
            }
            if self.cancel.is_cancelled() {
                return;
            }
            for queue_name in Queue::ALL {
                match queue::reclaim(&self.ctx.redis, *queue_name, &self.consumer, idle_ms, 100)
                    .await
                {
                    Ok(entries) => {
                        for (stream_id, body) in entries {
                            if self.cancel.is_cancelled() {
                                return;
                            }
                            tracing::info!(stream_id, "reclaimed abandoned job");
                            // The entry is already in the pending list
                            // (idle clock running from the reclaim), so
                            // a permit wait here must stay well inside
                            // `reclaim_idle_seconds` — refresh the idle
                            // clock while waiting.
                            match self
                                .wait_remote_permit(*queue_name, &stream_id, idle_ms)
                                .await
                            {
                                PermitWait::Acquired(p) => {
                                    self.dispatch(*queue_name, &stream_id, &body, Some(p)).await;
                                }
                                PermitWait::NotRequired => {
                                    self.dispatch(*queue_name, &stream_id, &body, None).await;
                                }
                                // No slot within the safe window — leave
                                // the entry pending; the next reclaim
                                // pass tries again. Never run a capped
                                // remote job without its permit.
                                PermitWait::TimedOut => continue,
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(queue = queue_name.as_str(), error = %e, "job reclaim failed");
                    }
                }
            }
        }
    }
}

/// The default consumer name: `{hostname}:{pid}`.
pub fn default_consumer() -> String {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{host}:{}", std::process::id())
}
