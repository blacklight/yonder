//! `/api/v1` — Mastodon-compatible REST API.
//!
//! Business routes land in epics 07–22: each adds its handlers here,
//! registers them on [`router`], and lists them in [`ApiDoc`]'s
//! `paths(...)`. Handlers carry `#[utoipa::path]`; keep them thin —
//! logic lives in domain services.

use std::sync::Arc;

use axum::Router;

use crate::http::AppState;

/// The v1 router. Merge feature subrouters here.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi()]
pub struct ApiDoc;
