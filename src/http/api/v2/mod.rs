//! `/api/v2` — Mastodon API v2 endpoints (added by feature epics,
//! e.g. `/api/v2/instance` in epic 06/22).

pub mod filters;
pub mod instance;
pub mod media;
pub mod search;
pub mod suggestions;

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
                    state.config().media.max_upload_size as usize + 64 * 1024,
                ))
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route("/api/v2/media/{id}", axum::routing::get(media::show))
        // Unlayered public read, like `/api/v1/instance`.
        .route("/api/v2/instance", axum::routing::get(instance::instance))
        .route(
            "/api/v2/search",
            axum::routing::get(search::search_v2).route_layer(ratelimit::layer(state, "search")),
        )
        // Epic 20 — server-side filters. Reads unlayered like other
        // account reads; writes carry the `relation` bucket.
        .route(
            "/api/v2/filters",
            axum::routing::get(filters::list)
                .post(filters::create)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v2/filters/{id}",
            axum::routing::get(filters::show)
                .put(filters::update)
                .delete(filters::delete_filter)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v2/filters/{id}/keywords",
            axum::routing::get(filters::keywords)
                .post(filters::add_keyword)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v2/filters/keywords/{id}",
            axum::routing::get(filters::show_keyword)
                .put(filters::update_keyword)
                .delete(filters::delete_keyword)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v2/filters/{id}/statuses",
            axum::routing::get(filters::statuses)
                .post(filters::add_status)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v2/filters/statuses/{id}",
            axum::routing::delete(filters::delete_status)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        // Epic 26 — follow suggestions (empty; no suggestion engine).
        .route("/api/v2/suggestions", axum::routing::get(suggestions::list))
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    instance::instance,
    media::upload,
    media::show,
    search::search_v2,
    filters::list,
    filters::create,
    filters::show,
    filters::update,
    filters::delete_filter,
    filters::keywords,
    filters::add_keyword,
    filters::show_keyword,
    filters::update_keyword,
    filters::delete_keyword,
    filters::statuses,
    filters::add_status,
    filters::delete_status,
    suggestions::list
))]
pub struct ApiDoc;
