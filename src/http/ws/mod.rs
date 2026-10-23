//! WebSocket streaming endpoints for live notifications/timelines.
//!
//! `/api/v1/streaming` implements Mastodon's protocol: clients upgrade
//! with `?access_token=` or an `Authorization` header (same token auth
//! as REST) and may pre-subscribe with `?stream=…` (plus `tag`/`list`
//! parameters). Connected clients send
//! `{"type":"subscribe","stream":"<name>", …}` /
//! `{"type":"unsubscribe","stream":"<name>", …}` text frames and receive
//! `{"stream":[…],"event":…,"payload":…}` frames — the wire stream name
//! is the *subscription* (`["user"]`, `["hashtag","foo"]`), never the
//! internal scoped key (`user:7`).
//!
//! `user*`, `direct` and `list*` subscriptions require authentication;
//! scoped delivery additionally only reaches the owning user's
//! connections (see [`registry::Connection::matching`]). The server
//! pings every [`HEARTBEAT_SECONDS`]s and client pings are echoed back
//! as pongs. Notification producers (epic 16) reach the bus through the
//! Redis [`relay`] — a standalone worker can push to live clients.

mod registry;
pub mod relay;

pub use registry::{
    ConnId, Connection, ConnectionRegistry, EventBus, StreamEvent, MAX_STREAM_NAME_LEN,
    MAX_SUBSCRIPTIONS,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    http::{
        middleware::auth::{self, AuthContext, TokenResolution},
        AppState,
    },
    Error,
};

/// Server heartbeat interval.
const HEARTBEAT_SECONDS: u64 = 30;

/// Close code sent when a private stream is requested without auth.
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
    params(("stream" = Option<String>, Query, description = "Stream to pre-subscribe to")),
    responses(
        (status = 101, description = "WebSocket upgrade"),
        (status = 401, description = "Invalid access token", body = crate::error::ErrorBody),
    )
)]
pub async fn streaming(
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = match auth::resolve_request(&state, &headers, &uri).await {
        TokenResolution::Authed(auth) => Some(auth),
        TokenResolution::Anonymous => None,
        TokenResolution::Rejected => return Error::Unauthorized.into_response(),
        TokenResolution::Failed(e) => return Error::Internal(e.to_string()).into_response(),
    };
    // Mastodon clients may pre-subscribe on the upgrade URL:
    // `?stream=user`, `?stream=hashtag&tag=rust`, `?stream=list&list=7`.
    let initial = params
        .get("stream")
        .filter(|s| !s.is_empty())
        .map(|s| subscription_name(s, &params));
    if let Some(stream) = &initial {
        if private_scoped(stream) && auth.is_none() {
            return Error::Unauthorized.into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_socket(state, socket, auth, initial))
}

/// One authenticated/anonymous socket's lifetime: a read loop
/// maintaining its subscriptions and a write task multiplexing bus
/// events, client-ping replies and the heartbeat.
async fn handle_socket(
    state: Arc<AppState>,
    socket: WebSocket,
    auth: Option<AuthContext>,
    initial: Option<String>,
) {
    let (conn_id, conn) = state.connections.add(auth.as_ref().map(|a| a.user_id));
    if let Some(stream) = initial {
        conn.subscribe(&stream);
    }
    let authed = auth.is_some();
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(64);
    let cancel = CancellationToken::new();

    let writer = {
        let state = state.clone();
        let conn = conn.clone();
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
                            if let Some(sub) = conn.matching(&ev.stream) {
                                let frame = json!({
                                    "stream": wire_stream(&sub),
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
                if !handle_client_message(&conn, authed, &text, &out_tx).await {
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
    state.connections.remove(conn_id);
}

/// Internal stream name for a subscription request. Mastodon clients
/// send `{"stream":"hashtag","tag":"rust"}` and
/// `{"stream":"list","list":"7"}` — the parameter is folded into the
/// internal name (`hashtag:rust`, `list:7`).
fn subscription_name(stream: &str, params: &HashMap<String, String>) -> String {
    match stream {
        "hashtag" | "hashtag:local" => params
            .get("tag")
            .map(|tag| format!("{stream}:{tag}"))
            .unwrap_or_else(|| stream.to_string()),
        "list" => params
            .get("list")
            .map(|id| format!("list:{id}"))
            .unwrap_or_else(|| stream.to_string()),
        _ => stream.to_string(),
    }
}

/// The wire form of an internal subscription name. Mastodon carries
/// parameterized streams as two array entries (`["hashtag","rust"]`,
/// `["list","7"]`); plain names are a single entry.
fn wire_stream(subscription: &str) -> Vec<String> {
    for prefix in ["hashtag:local:", "hashtag:", "list:"] {
        if let Some(param) = subscription.strip_prefix(prefix) {
            return vec![prefix.trim_end_matches(':').to_string(), param.to_string()];
        }
    }
    vec![subscription.to_string()]
}

/// Streams carrying private events — require authentication.
/// Ownership of `list:{id}` is enforced in epic 16 when the events are
/// published (the scoped match already prevents cross-user delivery of
/// `list:{id}:{uid}` events).
fn private_scoped(stream: &str) -> bool {
    stream == "user"
        || stream.starts_with("user:")
        || stream == "direct"
        || stream == "list"
        || stream.starts_with("list:")
}

/// Process one client text frame (`subscribe`/`unsubscribe`).
/// Returns `false` when the connection must close.
async fn handle_client_message(
    conn: &Connection,
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
    let params: HashMap<String, String> = msg
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let name = subscription_name(stream, &params);
    match kind {
        "subscribe" if !name.is_empty() => {
            if private_scoped(&name) && !authed {
                let _ = out_tx
                    .send(Outbound::Frame(Message::Close(Some(CloseFrame {
                        code: CLOSE_UNAUTHORIZED,
                        reason: "authentication required".into(),
                    }))))
                    .await;
                return false;
            }
            if !conn.subscribe(&name) {
                let _ = out_tx
                    .send(Outbound::Frame(Message::Text(
                        json!({"error": "subscription limit reached", "status": 400})
                            .to_string()
                            .into(),
                    )))
                    .await;
            }
        }
        "unsubscribe" if !name.is_empty() => conn.unsubscribe(&name),
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_subscription_scopes_events() {
        let registry = ConnectionRegistry::default();
        let (_id, alice) = registry.add(Some(7));
        let (_id, bob) = registry.add(Some(9));
        alice.subscribe("user");
        bob.subscribe("user");
        // Own events match — as the bare subscription name.
        assert_eq!(alice.matching("user:7").as_deref(), Some("user"));
        assert_eq!(alice.matching("user:9"), None);
        assert_eq!(bob.matching("user:9").as_deref(), Some("user"));
        // Public stream names only match their exact subscription.
        assert_eq!(alice.matching("public"), None);
        alice.subscribe("public:local");
        assert_eq!(
            alice.matching("public:local").as_deref(),
            Some("public:local")
        );
    }

    #[test]
    fn anonymous_connection_never_matches_user_streams() {
        let registry = ConnectionRegistry::default();
        let (_id, anon) = registry.add(None);
        anon.subscribe("user");
        assert_eq!(anon.matching("user").as_deref(), Some("user"));
        assert_eq!(anon.matching("user:7"), None);
    }

    #[test]
    fn unsubscribe_stops_delivery() {
        let registry = ConnectionRegistry::default();
        let (id, conn) = registry.add(Some(1));
        conn.subscribe("public");
        conn.unsubscribe("public");
        assert_eq!(conn.matching("public"), None);
        registry.remove(id);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn subscription_caps_apply() {
        let registry = ConnectionRegistry::default();
        let (_id, conn) = registry.add(None);
        for i in 0..MAX_SUBSCRIPTIONS {
            assert!(conn.subscribe(&format!("s{i}")));
        }
        assert!(!conn.subscribe("one-too-many"));
        assert!(!conn.subscribe(&"x".repeat(MAX_STREAM_NAME_LEN + 1)));
    }

    #[test]
    fn private_scoped_covers_user_direct_list() {
        for s in ["user", "user:notification", "direct", "list", "list:3"] {
            assert!(private_scoped(s), "{s} should require auth");
        }
        for s in ["public", "public:local", "hashtag", "hashtag:rust"] {
            assert!(!private_scoped(s), "{s} should be public");
        }
    }

    #[test]
    fn subscription_name_folds_params() {
        let params = HashMap::from([("tag".to_string(), "rust".to_string())]);
        assert_eq!(subscription_name("hashtag", &params), "hashtag:rust");
        let params = HashMap::from([("list".to_string(), "7".to_string())]);
        assert_eq!(subscription_name("list", &params), "list:7");
        assert_eq!(subscription_name("public", &HashMap::new()), "public");
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
