//! Connection registry and typed event bus for `/api/v1/streaming`.
//!
//! Producers (epic 16) publish [`StreamEvent`]s onto the [`EventBus`]
//! — an in-process `tokio::sync::broadcast` channel; cross-process
//! fan-out can slot behind the same type later. Each live socket holds
//! a [`ConnectionRegistry`] entry mapping it to its auth principal and
//! subscribed stream names.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;

/// Identifier for one live WebSocket connection.
pub type ConnId = Uuid;

/// Broadcast channel capacity — slow consumers get `Lagged` and skip
/// events rather than growing memory.
const BUS_CAPACITY: usize = 1024;

/// One event delivered to subscribed connections. `payload` serializes
/// inside the Mastodon frame
/// `{"stream": ["<stream>"], "event": "<event>", "payload": "<json>"}`.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    /// Stream name the event belongs to (`"public"`, `"user:{id}"`, …).
    pub stream: String,
    /// Mastodon event name (`update`, `notification`, `delete`, …).
    pub event: String,
    /// Event payload — a serialized entity.
    pub payload: Value,
}

/// Typed event bus shared through [`crate::http::AppState`].
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<StreamEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        EventBus { tx }
    }
}

impl EventBus {
    /// Publish an event to every connection subscribed to `stream`.
    pub fn publish(&self, stream: impl Into<String>, event: impl Into<String>, payload: Value) {
        let _ = self.tx.send(StreamEvent {
            stream: stream.into(),
            event: event.into(),
            payload,
        });
    }

    /// Subscribe a connection's write loop to the bus.
    pub fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
        self.tx.subscribe()
    }
}

/// What one connection is subscribed to and who owns it.
#[derive(Debug)]
struct ConnInfo {
    /// `users.id` behind the connection's token, if authenticated.
    user_id: Option<i64>,
    /// Stream names this connection subscribed to.
    streams: HashSet<String>,
}

/// Live WebSocket connections, by user and stream.
#[derive(Clone, Default)]
pub struct ConnectionRegistry {
    inner: Arc<Mutex<HashMap<ConnId, ConnInfo>>>,
}

impl ConnectionRegistry {
    /// Register a connection; returns its id.
    pub async fn add(&self, user_id: Option<i64>) -> ConnId {
        let id = Uuid::new_v4();
        self.inner.lock().await.insert(
            id,
            ConnInfo {
                user_id,
                streams: HashSet::new(),
            },
        );
        id
    }

    /// Drop a connection's registration.
    pub async fn remove(&self, id: ConnId) {
        self.inner.lock().await.remove(&id);
    }

    /// Subscribe `id` to `stream`. Returns `false` when the connection
    /// is unknown.
    pub async fn subscribe(&self, id: ConnId, stream: &str) -> bool {
        match self.inner.lock().await.get_mut(&id) {
            Some(info) => {
                info.streams.insert(stream.to_string());
                true
            }
            None => false,
        }
    }

    /// Unsubscribe `id` from `stream`.
    pub async fn unsubscribe(&self, id: ConnId, stream: &str) {
        if let Some(info) = self.inner.lock().await.get_mut(&id) {
            info.streams.remove(stream);
        }
    }

    /// Whether `id` should receive `event`'s stream. A `user`
    /// (or `user:notification`) subscription also matches the
    /// connection's own scoped events `user:{uid}`.
    pub async fn matches(&self, id: ConnId, event_stream: &str) -> bool {
        let inner = self.inner.lock().await;
        let Some(info) = inner.get(&id) else {
            return false;
        };
        info.streams.iter().any(|sub| {
            sub == event_stream
                || info
                    .user_id
                    .map(|uid| event_stream == format!("{sub}:{uid}"))
                    .unwrap_or(false)
        })
    }

    /// Number of live connections — diagnostics.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Whether no connections are live.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}
