//! `/api/v2/media` — Mastodon v2 media upload + processing-status GET
//! (epic 12). Uploads are synchronous, so the entity always carries
//! `processed` metadata already.

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, State},
    response::Json,
};
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    posts::{entity, PostContext},
    Error, Result,
};

/// `POST /api/v2/media` — same synchronous upload as v1.
#[utoipa::path(
    post,
    path = "/api/v2/media",
    tag = "media",
    request_body(content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "The media attachment"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 413, description = "Upload too large", body = crate::error::ErrorBody),
        (status = 422, description = "Unsupported type", body = crate::error::ErrorBody),
    )
)]
pub async fn upload(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    mp: Multipart,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:media")?;
    crate::http::api::v1::media::upload_inner(&state, &auth, mp).await
}

/// `GET /api/v2/media/{id}` — processing status (owner only).
#[utoipa::path(
    get,
    path = "/api/v2/media/{id}",
    tag = "media",
    params(("id" = String, Path, description = "Media id")),
    responses(
        (status = 200, description = "The media attachment"),
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
    let row = entity::load_media(&ctx, id)
        .await?
        .ok_or_else(|| Error::NotFound("media".into()))?;
    if row.account_id != auth.account_id {
        return Err(Error::NotFound("media".into()));
    }
    Ok(Json(entity::media_entity(&ctx.base_url(), &row)))
}
