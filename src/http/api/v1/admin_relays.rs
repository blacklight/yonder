//! `/api/v1/admin/relays*` — relay administration (epic 22).
//! Authorization is manual: relays start `pending`, `authorize` moves
//! them to `accepted`, and only accepted relays receive public-post
//! fan-out (`activitypub::delivery`). Logic lives in
//! [`crate::admin::relays`]; handlers stay thin.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    admin::relays,
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    Result,
};

/// `GET /api/v1/admin/relays` — every relay with its state.
#[utoipa::path(
    get,
    path = "/api/v1/admin/relays",
    tag = "admin",
    responses(
        (status = 200, description = "Array of relays"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(json!(relays::list(&state.db).await?)))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateBody {
    /// The relay's inbox URL (absolute https).
    pub inbox_url: String,
}

/// `POST /api/v1/admin/relays` — add a relay; starts `pending`.
#[utoipa::path(
    post,
    path = "/api/v1/admin/relays",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "The new relay, pending"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 409, description = "Relay already exists", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid inbox URL", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(
        relays::create(&state, auth.account_id, body.inbox_url.trim()).await?,
    ))
}

/// `POST /api/v1/admin/relays/{id}/authorize` — manual accept:
/// `pending` → `accepted`; fan-out starts.
#[utoipa::path(
    post,
    path = "/api/v1/admin/relays/{id}/authorize",
    tag = "admin",
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "The accepted relay"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Relay not pending", body = crate::error::ErrorBody),
    )
)]
pub async fn authorize(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(relays::authorize(&state, auth.account_id, id).await?))
}

/// `DELETE /api/v1/admin/relays/{id}` — remove a relay.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/relays/{id}",
    tag = "admin",
    params(("id" = i64, Path)),
    responses(
        (status = 200, description = "Relay removed"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such relay", body = crate::error::ErrorBody),
    )
)]
pub async fn remove(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    relays::remove(&state, auth.account_id, id).await?;
    Ok(Json(json!({})))
}
