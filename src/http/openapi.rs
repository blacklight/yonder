//! OpenAPI document assembly and serving.
//!
//! [`document`] merges every API module's fragment into one spec;
//! [`router`] serves it at `GET /openapi.json` and mounts Swagger-UI
//! at `/swagger-ui` (the epic-28 container deployment consumes the
//! same JSON).

use std::sync::Arc;

use axum::Router;
use utoipa::{
    openapi::{
        security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme},
        Components,
    },
    Modify, OpenApi,
};
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
    components(schemas(crate::error::ErrorBody)),
    modifiers(&SecuritySchemes),
    // `/` resolves relative to wherever the spec is served — the
    // instance itself, with no compile-time knowledge of its domain.
    servers((url = "/", description = "This instance"))
)]
struct ApiDoc;

/// The `bearer` security scheme Swagger-UI's Authorize dialog needs.
/// API clients authenticate with `Authorization: Bearer <token>`
/// where the token is an OAuth access token (`POST /oauth/token`).
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert(Components::default())
            .add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .description(Some(
                            "OAuth access token — obtain one through `POST /oauth/token`; \
                             paste the token itself (Swagger-UI adds the `Bearer ` prefix).",
                        ))
                        .build(),
                ),
            );
        // Document-level requirement: Swagger-UI only attaches
        // authorized credentials to operations whose security
        // requirement names an authorized scheme, and per-operation
        // annotation would mean touching every handler. Public and
        // MaybeAuth endpoints simply tolerate the extra header.
        openapi.security = Some(vec![SecurityRequirement::new(
            "bearer",
            Vec::<String>::new(),
        )]);
    }
}

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
