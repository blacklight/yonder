//! OpenAPI document assembly and serving.
//!
//! [`document`] merges every API module's fragment into one spec;
//! [`router`] serves it at `GET /openapi.json` and mounts Swagger-UI
//! at `/swagger-ui` (the epic-28 container deployment consumes the
//! same JSON).

use std::sync::Arc;

use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::http::{api, ws, AppState};

/// Root document: service-level paths plus every API module's
/// fragment. Feature epics register their `#[utoipa::path]` handlers
/// in their module's `ApiDoc`, not here.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Yonder API",
        version = env!("CARGO_PKG_VERSION"),
        description = "A Mastodon-compatible ActivityPub server",
        license(name = "AGPL-3.0-or-later")
    ),
    paths(crate::http::healthz, ws::streaming),
    components(schemas(crate::error::ErrorBody))
)]
struct ApiDoc;

/// The complete OpenAPI document.
pub fn document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
        .merge_from(api::v1::ApiDoc::openapi())
        .merge_from(api::v2::ApiDoc::openapi())
}

/// `/openapi.json` + `/swagger-ui`. `SwaggerUi::url` itself registers
/// the spec route, so no separate handler is needed.
pub fn router() -> Router<Arc<AppState>> {
    Router::<Arc<AppState>>::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/openapi.json", document()))
}
