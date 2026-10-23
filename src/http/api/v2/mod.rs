//! `/api/v2` — Mastodon API v2 endpoints (added by feature epics,
//! e.g. `/api/v2/instance` in epic 06/22).

pub mod media;
pub mod search;

use std::sync::Arc;

use axum::{routing::post, Router};

use crate::http::{middleware::ratelimit, AppState};

/// The v2 router. Merge feature subrouters here.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v2/media",
            post(media::upload)
                .route_layer(tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_upload_size as usize + 64 * 1024,
                ))
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route("/api/v2/media/{id}", axum::routing::get(media::show))
        .route(
            "/api/v2/search",
            axum::routing::get(search::search_v2).route_layer(ratelimit::layer(state, "search")),
        )
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi(paths(media::upload, media::show, search::search_v2))]
pub struct ApiDoc;
