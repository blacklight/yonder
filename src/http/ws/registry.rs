//! Connection registry and typed event bus for `/api/v1/streaming`.
//!
//! Producers (epic 16) publish [`StreamEvent`]s onto the [`EventBus`]
//! — an in-process `tokio::sync::broadcast` channel; cross-process
//! fan-out can slot behind the same type later. Each live socket holds
//! a [`Connection`] (reachable through [`ConnectionRegistry`] for
//! lifecycle/diagnostics) carrying its auth principal and subscribed
//! stream names.
//!
//! Subscriptions live on the `Connection` itself — an `Arc` shared by
//! the read and write halves — so per-event matching touches a small
//! per-connection lock, never the registry-wide map.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Identifier for one live WebSocket connection.
pub type ConnId = Uuid;

/// Broadcast channel capacity — slow consumers get `Lagged` and skip
/// events rather than growing memory.
const BUS_CAPACITY: usize = 1024;

/// Maximum subscriptions a single connection may hold.
pub const MAX_SUBSCRIPTIONS: usize = 64;

/// Maximum length of a stream name a client may subscribe to.
pub const MAX_STREAM_NAME_LEN: usize = 128;

/// One event delivered to subscribed connections. `payload` serializes
/// inside the Mastodon frame
/// `{"stream": ["<stream>"], "event": "<event>", "payload": "<json>"}`.
/// Job-side producers ship it over Redis pub/sub to [`super::relay`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// One live connection: the owning user (if authenticated) and its
/// stream subscriptions.
///
/// `streams` uses `std::sync::Mutex` — critical sections are a single
/// set insert/remove/scan, never held across `.await`.
#[derive(Debug)]
pub struct Connection {
    user_id: Option<i64>,
    streams: Mutex<HashSet<String>>,
}

impl Connection {
    fn new(user_id: Option<i64>) -> Self {
        Connection {
            user_id,
            streams: Mutex::new(HashSet::new()),
        }
    }

    /// `users.id` behind the connection's token, if authenticated.
    pub fn user_id(&self) -> Option<i64> {
        self.user_id
    }

    /// Subscribe to `stream`. Returns `false` when the name exceeds
    /// [`MAX_STREAM_NAME_LEN`] or the [`MAX_SUBSCRIPTIONS`] cap is
    /// already reached.
    pub fn subscribe(&self, stream: &str) -> bool {
        let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        if stream.len() > MAX_STREAM_NAME_LEN
            || (!streams.contains(stream) && streams.len() >= MAX_SUBSCRIPTIONS)
        {
            return false;
        }
        streams.insert(stream.to_string());
        true
    }

    /// Unsubscribe from `stream`.
    pub fn unsubscribe(&self, stream: &str) {
        let mut streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        streams.remove(stream);
    }

    /// The subscribed stream name matching `event_stream`, if any.
    ///
    /// Exact names match directly; a scoped event (`user:{uid}`,
    /// `list:{id}:{uid}`, …) additionally matches a bare-name
    /// subscription on connections owned by that user. The returned
    /// name — the *subscription*, not the scoped event stream — is
    /// what goes on the wire: an event on `user:7` reaches a `user`
    /// subscriber as `["user"]`.
    pub fn matching(&self, event_stream: &str) -> Option<String> {
        let streams = self.streams.lock().unwrap_or_else(|p| p.into_inner());
        if streams.contains(event_stream) {
            return Some(event_stream.to_string());
        }
        let uid = self.user_id?;
        streams
            .iter()
            .find(|sub| {
                event_stream.len() > sub.len() + 1
                    && event_stream.starts_with(sub.as_str())
                    && event_stream[sub.len()..] == format!(":{uid}")
            })
            .cloned()
    }
}

/// Live WebSocket connections, keyed by id. The map lock is only held
/// for add/remove/lookup — subscription matching happens on the
/// per-connection [`Connection`].
#[derive(Clone, Default)]
pub struct ConnectionRegistry {
    inner: Arc<Mutex<HashMap<ConnId, Arc<Connection>>>>,
}

impl ConnectionRegistry {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<ConnId, Arc<Connection>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Register a connection; returns its id and handle.
    pub fn add(&self, user_id: Option<i64>) -> (ConnId, Arc<Connection>) {
        let id = Uuid::new_v4();
        let conn = Arc::new(Connection::new(user_id));
        self.map().insert(id, conn.clone());
        (id, conn)
    }

    /// Drop a connection's registration.
    pub fn remove(&self, id: ConnId) {
        self.map().remove(&id);
    }

    /// Look up a connection's handle.
    pub fn get(&self, id: ConnId) -> Option<Arc<Connection>> {
        self.map().get(&id).cloned()
    }

    /// Number of live connections — diagnostics.
    pub fn len(&self) -> usize {
        self.map().len()
    }

    /// Whether no connections are live.
    pub fn is_empty(&self) -> bool {
        self.map().is_empty()
    }
}
