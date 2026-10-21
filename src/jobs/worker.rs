//! The worker: claims jobs from every queue, dispatches them to the
//! registered handlers, acks on success, and retries with exponential
//! backoff before dead-lettering. A delayed-job poller and an
//! `XAUTOCLAIM` reclaimer run alongside it.
//!
//! Run it in-process via `jobs.enabled` on `serve`, or standalone via
//! `yonder worker`.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{
    config::JobsConfig,
    jobs::{queue, JobContext, JobEnvelope, Queue, Registry},
    Result,
};

/// A worker draining every [`Queue`] plus the delayed-job ZSET.
#[derive(Clone)]
pub struct Worker {
    ctx: JobContext,
    registry: Arc<Registry>,
    jobs: Arc<JobsConfig>,
    consumer: String,
    cancel: CancellationToken,
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
        Worker {
            ctx,
            registry,
            jobs,
            consumer: consumer.into(),
            cancel: CancellationToken::new(),
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
            for _ in 0..self.jobs.concurrency {
                let worker = self.clone();
                set.spawn(async move { worker.consumer_loop(*queue_name).await });
            }
        }
        let delayed = self.clone();
        set.spawn(async move { delayed.delayed_loop().await });
        let reclaimer = self.clone();
        set.spawn(async move { reclaimer.reclaim_loop().await });
        self.cancel.cancelled().await;
        set.shutdown().await;
        Ok(())
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
                        self.dispatch(queue_name, &stream_id, &body).await;
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
    /// fail with retry/dead-letter semantics.
    async fn dispatch(&self, queue_name: Queue, stream_id: &str, body: &str) {
        let envelope = match queue::decode(body) {
            Ok(envelope) => envelope,
            Err(e) => {
                tracing::warn!(stream_id, error = %e, "acking malformed job entry");
                let _ = queue::ack(&self.ctx.redis, queue_name, stream_id).await;
                return;
            }
        };
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
        match handler.run(&self.ctx, &envelope.payload).await {
            Ok(()) => {
                if let Err(e) = queue::ack(&self.ctx.redis, queue_name, stream_id).await {
                    tracing::warn!(stream_id, error = %e, "job ack failed");
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
                            tracing::info!(stream_id, "reclaimed abandoned job");
                            self.dispatch(*queue_name, &stream_id, &body).await;
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
