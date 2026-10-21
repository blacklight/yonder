//! `/api/v2` — Mastodon API v2 endpoints (added by feature epics,
//! e.g. `/api/v2/instance` in epic 06/22).

use std::sync::Arc;

use axum::Router;

use crate::http::AppState;

/// The v2 router. Merge feature subrouters here.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi()]
pub struct ApiDoc;
