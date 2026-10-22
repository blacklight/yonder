//! `/api/v1` — Mastodon-compatible REST API.
//!
//! Business routes land in epics 07–22: each adds its handlers here,
//! registers them on [`router`], and lists them in [`ApiDoc`]'s
//! `paths(...)`. Handlers carry `#[utoipa::path]`; keep them thin —
//! logic lives in domain services.

pub mod accounts;
pub mod apps;
pub mod instance;
pub mod sessions;
pub mod settings;
pub mod tokens;
pub mod two_factor;

use std::sync::Arc;

use axum::{routing::get, routing::post, Router};

use crate::http::{middleware::ratelimit, AppState};

/// The v1 router. Merge feature subrouters here.
///
/// Layer order matters: `route_layer` wraps only the routes registered
/// *before* it — registration is added last so it counts solely against
/// `account_creation`, not both buckets.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/accounts/verify_email",
            post(accounts::verify_email),
        )
        .route(
            "/api/v1/accounts/verify_credentials",
            get(accounts::verify_credentials),
        )
        // Profile editing (epic 09). Multipart bypasses
        // `DefaultBodyLimit`, so the body cap rides on the route itself
        // (two uploads + slack).
        .route(
            "/api/v1/accounts/update_credentials",
            axum::routing::patch(accounts::update_credentials).route_layer(
                tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_avatar_size as usize * 2 + 64 * 1024,
                ),
            ),
        )
        .route("/api/v1/apps", post(apps::create_app))
        .route(
            "/api/v1/apps/verify_credentials",
            get(apps::verify_credentials),
        )
        .route(
            "/api/v1/oauth/authorized_applications",
            get(apps::authorized_applications),
        )
        .route(
            "/api/v1/oauth/authorized_applications/{id}",
            axum::routing::delete(apps::revoke_authorization),
        )
        .route(
            "/api/v1/tokens",
            post(tokens::create_token).get(tokens::list_tokens),
        )
        .route(
            "/api/v1/tokens/{id}",
            axum::routing::delete(tokens::revoke_token),
        )
        .route("/api/v1/sessions", get(sessions::list_sessions))
        .route(
            "/api/v1/sessions/{id}",
            axum::routing::delete(sessions::revoke_session),
        )
        // 2FA management (epic 08) — inside the `authentication` bucket.
        .route(
            "/api/v1/2fa",
            get(two_factor::status).delete(two_factor::disable),
        )
        .route("/api/v1/2fa/totp/setup", post(two_factor::totp_setup))
        .route("/api/v1/2fa/totp/confirm", post(two_factor::totp_confirm))
        .route(
            "/api/v1/2fa/totp",
            axum::routing::delete(two_factor::totp_delete),
        )
        .route("/api/v1/2fa/backup_codes", post(two_factor::backup_codes))
        .route(
            "/api/v1/2fa/webauthn/register/begin",
            post(two_factor::webauthn_register_begin),
        )
        .route(
            "/api/v1/2fa/webauthn/register",
            post(two_factor::webauthn_register),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials",
            get(two_factor::webauthn_credentials),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials/{id}/rename",
            post(two_factor::webauthn_rename),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials/{id}",
            axum::routing::delete(two_factor::webauthn_delete),
        )
        .route("/api/v1/instance", get(instance::instance))
        // User settings (epic 10).
        .route("/api/v1/preferences", get(settings::preferences))
        .route(
            "/api/v1/yonder/settings",
            get(settings::get_settings).patch(settings::patch_settings),
        )
        // The `authentication` bucket covers every auth-adjacent
        // endpoint registered above.
        .route_layer(ratelimit::layer(state, "authentication"))
        // Registration gets only the dedicated `account_creation`
        // bucket — added after `route_layer` so it isn't double-counted.
        .route(
            "/api/v1/accounts",
            post(accounts::register).route_layer(ratelimit::layer(state, "account_creation")),
        )
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    accounts::register,
    accounts::verify_credentials,
    accounts::update_credentials,
    accounts::verify_email,
    apps::create_app,
    apps::verify_credentials,
    apps::authorized_applications,
    apps::revoke_authorization,
    tokens::create_token,
    tokens::list_tokens,
    tokens::revoke_token,
    sessions::list_sessions,
    sessions::revoke_session,
    two_factor::status,
    two_factor::totp_setup,
    two_factor::totp_confirm,
    two_factor::totp_delete,
    two_factor::backup_codes,
    two_factor::webauthn_register_begin,
    two_factor::webauthn_register,
    two_factor::webauthn_credentials,
    two_factor::webauthn_rename,
    two_factor::webauthn_delete,
    two_factor::disable,
    instance::instance,
    settings::preferences,
    settings::get_settings,
    settings::patch_settings
))]
pub struct ApiDoc;
