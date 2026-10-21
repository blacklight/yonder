//! WebSocket streaming endpoints for live notifications/timelines.
//!
//! `/api/v1/streaming` implements Mastodon's protocol: clients upgrade
//! with `?access_token=` or an `Authorization` header (same token auth
//! as REST), then send `{"type":"subscribe","stream":"<name>"}` /
//! `{"type":"unsubscribe","stream":"<name>"}` text frames and receive
//! `{"stream":[…],"event":…,"payload":…}` frames. `user` and
//! `user:notification` subscriptions require authentication. The
//! server pings every [`HEARTBEAT_SECONDS`]s and client pings are
//! echoed back as pongs. Event producers arrive in epic 16 — the
//! transport, registry and bus live here.

mod registry;

pub use registry::{ConnId, ConnectionRegistry, EventBus, StreamEvent};

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::http::{
    middleware::auth::{self, AuthContext},
    AppState,
};

/// Server heartbeat interval.
const HEARTBEAT_SECONDS: u64 = 30;

/// Close code sent when a `user*` stream is requested without auth.
const CLOSE_UNAUTHORIZED: u16 = 4401;

/// Messages the read loop hands to the write task.
enum Outbound {
    /// Flush the sink — used to push out tungstenite's auto-queued
    /// pong replies without sending a second, explicit pong.
    Flush,
    /// A real frame to send.
    Frame(Message),
}

/// `GET /api/v1/streaming` — upgrade to the live event socket.
///
/// Token resolution is shared with REST: a presented-but-invalid
/// token rejects the upgrade with HTTP 401; no token connects
/// anonymously (public streams only).
#[utoipa::path(
    get,
    path = "/api/v1/streaming",
    tag = "streaming",
    responses(
        (status = 101, description = "WebSocket upgrade"),
        (status = 401, description = "Invalid access token", body = crate::error::ErrorBody),
    )
)]
pub async fn streaming(
    headers: HeaderMap,
    uri: Uri,
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = match auth::bearer_token(&headers, &uri) {
        Some(token) => match auth::resolve_token(&state, &token).await {
            Ok(auth) => Some(auth),
            Err(e) => return e.into_response(),
        },
        None => None,
    };
    ws.on_upgrade(move |socket| handle_socket(state, socket, auth))
}

/// One authenticated/anonymous socket's lifetime: a read loop
/// maintaining its subscriptions and a write task multiplexing bus
/// events, client-ping replies and the heartbeat.
async fn handle_socket(state: Arc<AppState>, socket: WebSocket, auth: Option<AuthContext>) {
    let conn_id = state
        .connections
        .add(auth.as_ref().map(|a| a.user_id))
        .await;
    let authed = auth.is_some();
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(64);
    let cancel = CancellationToken::new();

    let writer = {
        let state = state.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut rx = state.bus.subscribe();
            let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
            heartbeat.tick().await; // first tick fires immediately — skip it
            loop {
                // Biased: queued outbound frames (pongs, close) always
                // flush before a pending cancellation wins — a `Close`
                // enqueued just before `cancel.cancel()` must reach the
                // client.
                tokio::select! {
                    biased;
                    msg = out_rx.recv() => match msg {
                        Some(Outbound::Flush) => {
                            if sink.flush().await.is_err() { break }
                        }
                        Some(Outbound::Frame(m)) => {
                            let closing = matches!(m, Message::Close(_));
                            if sink.send(m).await.is_err() || closing { break }
                        }
                        None => break,
                    },
                    ev = rx.recv() => match ev {
                        Ok(ev) => {
                            if state.connections.matches(conn_id, &ev.stream).await {
                                let frame = json!({
                                    "stream": [ev.stream],
                                    "event": ev.event,
                                    // Mastodon sends the entity as a
                                    // JSON-encoded string.
                                    "payload": ev.payload.to_string(),
                                });
                                if sink
                                    .send(Message::Text(frame.to_string().into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(conn = %conn_id, skipped = n, "stream client lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = heartbeat.tick() => {
                        if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        })
    };

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                if !handle_client_message(&state, conn_id, authed, &text, &out_tx).await {
                    break;
                }
            }
            // Tungstenite auto-queues a pong on read, but nothing
            // flushes it while the halves are split — nudge the writer.
            Message::Ping(_) => {
                if out_tx.send(Outbound::Flush).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    cancel.cancel();
    let _ = writer.await;
    state.connections.remove(conn_id).await;
}

/// Process one client text frame (`subscribe`/`unsubscribe`).
/// Returns `false` when the connection must close.
async fn handle_client_message(
    state: &Arc<AppState>,
    conn_id: ConnId,
    authed: bool,
    text: &str,
    out_tx: &mpsc::Sender<Outbound>,
) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return true; // unparseable frame: ignore
    };
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or_default();
    let stream = msg
        .get("stream")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "subscribe" if !stream.is_empty() => {
            if user_scoped(stream) && !authed {
                let _ = out_tx
                    .send(Outbound::Frame(Message::Close(Some(CloseFrame {
                        code: CLOSE_UNAUTHORIZED,
                        reason: "authentication required".into(),
                    }))))
                    .await;
                return false;
            }
            state.connections.subscribe(conn_id, stream).await;
        }
        "unsubscribe" if !stream.is_empty() => {
            state.connections.unsubscribe(conn_id, stream).await;
        }
        _ => {}
    }
    true
}

/// Streams carrying private events — require authentication.
fn user_scoped(stream: &str) -> bool {
    stream == "user" || stream.starts_with("user:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn user_subscription_scopes_events() {
        let registry = ConnectionRegistry::default();
        let alice = registry.add(Some(7)).await;
        let bob = registry.add(Some(9)).await;
        registry.subscribe(alice, "user").await;
        registry.subscribe(bob, "user").await;
        // Own events match; another user's don't.
        assert!(registry.matches(alice, "user:7").await);
        assert!(!registry.matches(alice, "user:9").await);
        assert!(registry.matches(bob, "user:9").await);
        // Public stream names only match their exact subscription.
        assert!(!registry.matches(alice, "public").await);
        registry.subscribe(alice, "public:local").await;
        assert!(registry.matches(alice, "public:local").await);
    }

    #[tokio::test]
    async fn anonymous_connection_never_matches_user_streams() {
        let registry = ConnectionRegistry::default();
        let anon = registry.add(None).await;
        registry.subscribe(anon, "user").await;
        assert!(registry.matches(anon, "user").await);
        assert!(!registry.matches(anon, "user:7").await);
    }

    #[tokio::test]
    async fn unsubscribe_stops_delivery() {
        let registry = ConnectionRegistry::default();
        let id = registry.add(Some(1)).await;
        registry.subscribe(id, "public").await;
        registry.unsubscribe(id, "public").await;
        assert!(!registry.matches(id, "public").await);
        registry.remove(id).await;
        assert!(!registry.matches(id, "public").await);
        assert_eq!(registry.len().await, 0);
    }

    #[test]
    fn user_scoped_covers_user_streams_only() {
        assert!(user_scoped("user"));
        assert!(user_scoped("user:notification"));
        assert!(!user_scoped("public"));
        assert!(!user_scoped("list"));
    }

    #[test]
    fn bus_fanout() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.publish("public", "update", json!({"id": 1}));
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.stream, "public");
        assert_eq!(ev.event, "update");
    }
}
