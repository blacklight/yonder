//! `/api/v1/yonder/admin/settings*` — config-overlay CRUD (epic 22),
//! plus `/api/v1/yonder/admin/frontend` (default frontend selection)
//! and `/api/v1/yonder/admin/webpush/generate_keys` (VAPID rotation).
//! Admin-only; every mutation writes one `audit_log` row. The domain
//! logic lives in [`crate::admin::settings`] and
//! [`crate::admin::webpush`] — handlers only authorize and audit.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    admin::{settings, webpush},
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    moderation::audit,
    Result,
};

#[derive(Deserialize)]
pub struct ListQuery {
    /// Narrow the listing to one top-level config section.
    section: Option<String>,
}

/// `GET /api/v1/yonder/admin/settings` — every config key with its
/// TOML path, env var, effective value and source for UI tooltips.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/admin/settings",
    tag = "admin",
    params(("section" = Option<String>, Query, description = "Top-level config section filter")),
    responses(
        (status = 200, description = "Settings entries"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn list_settings(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(json!(
        settings::describe_all(&state, q.section.as_deref()).await?
    )))
}

/// `GET /api/v1/yonder/admin/settings/{key}` — one key's description.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/admin/settings/{key}",
    tag = "admin",
    params(("key" = String, Path, description = "Dotted TOML path, e.g. `instance.name`")),
    responses(
        (status = 200, description = "Setting entry"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown key", body = crate::error::ErrorBody),
    )
)]
pub async fn show_setting(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(key): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(settings::describe(&state, &key).await?))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetBody {
    /// New overlay value; must deserialize into the key's type.
    pub value: Value,
}

/// `PUT /api/v1/yonder/admin/settings/{key}` — set `key` as a DB
/// overlay; the effective config reloads immediately.
#[utoipa::path(
    put,
    path = "/api/v1/yonder/admin/settings/{key}",
    tag = "admin",
    request_body(content_type = "application/json"),
    params(("key" = String, Path, description = "Dotted TOML path")),
    responses(
        (status = 200, description = "Updated setting entry"),
        (status = 403, description = "Admin role required or read-only section", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown key", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid value", body = crate::error::ErrorBody),
    )
)]
pub async fn set_setting(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(key): Path<String>,
    Json(body): Json<SetBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let sensitive = settings::is_sensitive(&key);
    let entry = settings::validate_and_apply(&state, &key, body.value.clone()).await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "config.set",
        "config",
        None,
        json!({
            "key": key,
            "value": if sensitive { Value::from("<redacted>") } else { body.value },
        }),
    )
    .await?;
    Ok(Json(entry))
}

/// `DELETE /api/v1/yonder/admin/settings/{key}` — remove the overlay;
/// the effective value falls back to env → TOML → default.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/admin/settings/{key}",
    tag = "admin",
    params(("key" = String, Path, description = "Dotted TOML path")),
    responses(
        (status = 200, description = "Setting entry after reset"),
        (status = 403, description = "Admin role required or read-only section", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown key", body = crate::error::ErrorBody),
    )
)]
pub async fn reset_setting(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(key): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let entry = settings::reset(&state, &key).await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "config.reset",
        "config",
        None,
        json!({ "key": key }),
    )
    .await?;
    Ok(Json(entry))
}

/// `GET /api/v1/yonder/admin/frontend` — the default frontend setting
/// (`admin.default_frontend`) as a regular settings entry. Selecting
/// it is an overlay PUT — frontend *installation* lands in epic 23.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/admin/frontend",
    tag = "admin",
    responses(
        (status = 200, description = "The admin.default_frontend setting entry"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn get_frontend(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(
        settings::describe(&state, "admin.default_frontend").await?,
    ))
}

/// `PUT /api/v1/yonder/admin/frontend` — set the default frontend;
/// the value must name an installed frontend (or the builtin
/// `yonder`), validated against the epic-23 registry.
#[utoipa::path(
    put,
    path = "/api/v1/yonder/admin/frontend",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "Updated setting entry"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid frontend name", body = crate::error::ErrorBody),
    )
)]
pub async fn set_frontend(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<SetBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let name = body.value.as_str().unwrap_or("");
    if crate::frontends::find(&state.config(), name)?.is_none() {
        return Err(crate::Error::Validation(format!(
            "unknown frontend {name:?}"
        )));
    }
    let entry = settings::validate_and_apply(&state, "admin.default_frontend", body.value).await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "config.set",
        "config",
        None,
        json!({ "key": "admin.default_frontend", "value": entry["value"] }),
    )
    .await?;
    Ok(Json(entry))
}

/// `POST /api/v1/yonder/admin/webpush/generate_keys` — mint a VAPID
/// keypair and store it as overlays. Returns the public key only.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/admin/webpush/generate_keys",
    tag = "admin",
    responses(
        (status = 200, description = "The new public key and subject"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn generate_webpush_keys(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(
        webpush::generate_and_store(&state, Some(auth.account_id)).await?,
    ))
}
