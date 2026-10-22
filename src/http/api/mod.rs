//! Mastodon-compatible REST API + `yonder` extension payloads.
//!
//! [`router`] composes the versioned subrouters. Feature epics own
//! their route modules under `v1`/`v2`; the `/api/v1/streaming`
//! WebSocket lives here because it shares the API's auth and
//! rate-limit conventions.

pub mod v1;
pub mod v2;

use std::sync::Arc;

use axum::{routing::get, Router};

use crate::http::{middleware::ratelimit, ws, AppState};

/// The `/api` router tree.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/streaming", get(ws::streaming))
        // Token resolution is an authentication operation — the
        // `authentication` bucket guards connection attempts.
        .route_layer(ratelimit::layer(state, "authentication"))
        .merge(v1::router(state))
        .merge(v2::router(state))
}
