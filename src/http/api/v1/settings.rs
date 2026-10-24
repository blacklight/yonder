//! `/api/v1/preferences` (Mastodon-compatible read) and
//! `/api/v1/yonder/settings` (extension GET/PATCH) — the epic-10
//! user-settings API. Domain logic lives in [`crate::users::settings`];
//! handlers stay thin.

use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, Json};
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    users::settings,
    Result,
};

/// `GET /api/v1/preferences` — the Mastodon Preferences entity. Native
/// keys at top level; nonstandard settings under `yonder` (same object
/// as `GET /api/v1/yonder/settings`).
#[utoipa::path(
    get,
    path = "/api/v1/preferences",
    tag = "settings",
    responses(
        (status = 200, description = "The authenticated user's preferences"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
    )
)]
pub async fn preferences(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    let user_settings = settings::load(&state.db, auth.user_id).await?;
    Ok(Json(user_settings.preferences_json(
        &state.config(),
        settings::accept_language(&headers).as_deref(),
    )))
}

/// `GET /api/v1/yonder/settings` — every non-Mastodon-native setting,
/// in the same shape as `CredentialAccount.yonder.settings`.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/settings",
    tag = "settings",
    responses(
        (status = 200, description = "The authenticated user's yonder settings"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
    )
)]
pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    let user_settings = settings::load(&state.db, auth.user_id).await?;
    Ok(Json(user_settings.yonder_json(
        &state.config(),
        settings::accept_language(&headers).as_deref(),
    )))
}

/// OpenAPI shape of `PATCH /api/v1/yonder/settings` (all fields
/// optional; `null` resets to the BOOTSTRAP default).
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SettingsPatchSchema {
    /// UI language (`en`/`es`/`it`/`de`/`ar`; `null` = follow system).
    pub interface_language: Option<String>,
    /// Default posting language (same set; `null` = app language).
    pub default_posting_language: Option<String>,
    /// Default post visibility.
    pub default_visibility: Option<String>,
    /// Mark new statuses sensitive by default.
    pub default_sensitive: Option<bool>,
    /// `light`/`dark`/`system` (`null` = system).
    pub theme: Option<String>,
    /// `#rrggbb` accent color (`null` = instance default).
    pub accent: Option<String>,
    /// Drop notifications from strangers.
    pub block_stranger_notifications: Option<bool>,
    /// Auto-delete posts after N days (`0`/`null` = never).
    pub scheduled_deletion_days: Option<i32>,
    /// Show which app authored a post.
    pub show_app_display: Option<bool>,
    /// Include public posts in search results.
    pub include_public_posts_in_search: Option<bool>,
    /// Opt into the instance profile directory.
    pub directory_opt_in: Option<bool>,
    /// Always expand CW'd content (`false` = show warning).
    pub cw_always_expand: Option<bool>,
    /// Hide posts in these languages from timelines.
    pub filter_languages: Option<Vec<String>>,
    /// Per-event email notification toggles (partial objects merge).
    pub email_notifications: Option<EmailNotificationsSchema>,
}

/// The `email_notifications` object shape.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct EmailNotificationsSchema {
    pub follow: Option<bool>,
    pub follow_request: Option<bool>,
    pub reblog: Option<bool>,
    pub favourite: Option<bool>,
    pub mention: Option<bool>,
    pub quote: Option<bool>,
}

/// `PATCH /api/v1/yonder/settings` — partial update; returns the same
/// object as the GET. Unknown keys and invalid values are 422.
#[utoipa::path(
    patch,
    path = "/api/v1/yonder/settings",
    tag = "settings",
    request_body = SettingsPatchSchema,
    responses(
        (status = 200, description = "Updated yonder settings"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn patch_settings(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let patch = settings::parse_patch(&body)?;
    settings::save(&state.db, &auth, &patch).await?;
    let user_settings = settings::load(&state.db, auth.user_id).await?;
    Ok(Json(user_settings.yonder_json(
        &state.config(),
        settings::accept_language(&headers).as_deref(),
    )))
}
