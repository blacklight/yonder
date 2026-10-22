//! `/api/v1/scheduled_statuses` — list/get/reschedule/cancel (epic 12).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::Json,
};
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    posts::{entity, scheduled, PostContext},
    Error, Result,
};

/// `GET /api/v1/scheduled_statuses` — the caller's scheduled posts.
#[utoipa::path(
    get,
    path = "/api/v1/scheduled_statuses",
    tag = "scheduled_statuses",
    responses(
        (status = 200, description = "Scheduled statuses, soonest first"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:statuses")?;
    let ctx = PostContext::new(&state);
    let rows = scheduled::list(&ctx, auth.account_id).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(entity::scheduled_status_entity(&ctx, row).await?);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/scheduled_statuses/{id}` — one scheduled post.
#[utoipa::path(
    get,
    path = "/api/v1/scheduled_statuses/{id}",
    tag = "scheduled_statuses",
    params(("id" = String, Path, description = "Scheduled status id")),
    responses(
        (status = 200, description = "The scheduled status"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:statuses")?;
    let ctx = PostContext::new(&state);
    match scheduled::load(&ctx, id).await? {
        Some(row) if row.account_id == auth.account_id => {
            Ok(Json(entity::scheduled_status_entity(&ctx, &row).await?))
        }
        _ => Err(Error::NotFound("scheduled status".into())),
    }
}

/// `PUT /api/v1/scheduled_statuses/{id}` — reschedule; a stale earlier
/// job is neutralized by the publish handler's due-time check.
#[utoipa::path(
    put,
    path = "/api/v1/scheduled_statuses/{id}",
    tag = "scheduled_statuses",
    params(("id" = String, Path, description = "Scheduled status id")),
    request_body(content_type = "application/json", content = RescheduleSchema),
    responses(
        (status = 200, description = "The rescheduled status"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid scheduled_at", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:statuses")?;
    let scheduled_at = body
        .get("scheduled_at")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
        .ok_or_else(|| Error::Validation("scheduled_at must be an RFC-3339 timestamp".into()))?;
    let ctx = PostContext::new(&state);
    let row = scheduled::update(&ctx, auth.account_id, id, scheduled_at).await?;
    Ok(Json(entity::scheduled_status_entity(&ctx, &row).await?))
}

/// `DELETE /api/v1/scheduled_statuses/{id}` — cancel; media
/// reservations release via cascade.
#[utoipa::path(
    delete,
    path = "/api/v1/scheduled_statuses/{id}",
    tag = "scheduled_statuses",
    params(("id" = String, Path, description = "Scheduled status id")),
    responses(
        (status = 200, description = "Cancelled"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    scheduled::cancel(&ctx, auth.account_id, id).await?;
    Ok(Json(serde_json::json!({})))
}

/// Reschedule body shape.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RescheduleSchema {
    /// New publish time (RFC-3339, must be in the future).
    pub scheduled_at: String,
}
