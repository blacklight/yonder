//! The Redis→`EventBus` relay (epic 16): job-side producers publish
//! `StreamEvent`s to the `{prefix}:stream` pub/sub channel and each
//! serving process forwards them onto its local [`EventBus`]. This is
//! the single live-push path for job-generated events — it never
//! double-delivers, and the standalone worker (no live bus, detached
//! by design) reaches WebSocket clients just the same.
//!
//! Failure handling matches the codebase's fail-soft posture: decode
//! failures drop a message, connection failures reconnect with
//! exponential backoff, and the task runs forever — losing the relay
//! degrades live push to "rows only" instead of killing the server.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::{cache::Redis, http::ws::EventBus, Result};

/// Logical channel name — the physical Redis channel is
/// `redis.key(CHANNEL)` (`{prefix}:stream`).
pub const CHANNEL: &str = "stream";

/// Run the relay until `cancel` fires. Reconnects on every failure.
pub async fn run(redis: Redis, bus: EventBus, cancel: CancellationToken) {
    let channel = redis.key(CHANNEL);
    let mut backoff = Duration::from_millis(200);
    loop {
        let outcome = tokio::select! {
            result = listen(&redis, &bus, &channel) => result,
            _ = cancel.cancelled() => return,
        };
        match outcome {
            // `listen` only returns when the message stream ended —
            // either way we reconnect after a pause.
            Ok(()) => tracing::warn!("stream relay channel closed; reconnecting"),
            Err(e) => tracing::warn!(error = %e, "stream relay error; reconnecting"),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancel.cancelled() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One subscription lifetime: subscribe, then forward every message
/// onto the local bus until the stream ends or errors.
async fn listen(redis: &Redis, bus: &EventBus, channel: &str) -> Result<()> {
    let mut pubsub = redis.pubsub().await?;
    pubsub.subscribe(channel).await?;
    let mut messages = pubsub.on_message();
    while let Some(msg) = messages.next().await {
        let payload: String = msg.get_payload()?;
        match serde_json::from_str::<super::StreamEvent>(&payload) {
            Ok(ev) => bus.publish(ev.stream, ev.event, ev.payload),
            Err(e) => {
                tracing::warn!(error = %e, "stream relay dropped undecodable message")
            }
        }
    }
    Ok(())
}
