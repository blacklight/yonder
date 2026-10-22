//! `/api/v1/media` — upload (multipart) and description/focus update
//! (epic 12). Business logic lives in `media::attachments`; handlers
//! stay thin.

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Request, State},
    response::Json,
};
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    media::attachments::{parse_focus, store_status_media, UploadMeta},
    posts::{self, entity, PostContext},
    Error, Result,
};

/// `POST /api/v1/media` — multipart `file` + optional `description` /
/// `focus`. Synchronous (image processing is inline).
#[utoipa::path(
    post,
    path = "/api/v1/media",
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
    upload_inner(&state, &auth, mp).await
}

pub(crate) async fn upload_inner(
    state: &Arc<AppState>,
    auth: &AuthContext,
    mut mp: Multipart,
) -> Result<Json<Value>> {
    let mut file: Option<(Vec<u8>, Option<String>)> = None;
    let mut description = None;
    let mut focus = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart body: {e}")))?
    {
        match field.name() {
            Some("file") => {
                let ct = field.content_type().map(str::to_string);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| Error::Validation(format!("failed to read upload: {e}")))?;
                file = Some((bytes.to_vec(), ct));
            }
            Some("description") => {
                description = Some(field.text().await.unwrap_or_default());
            }
            Some("focus") => {
                focus = Some(field.text().await.unwrap_or_default());
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let Some((bytes, ct)) = file else {
        return Err(Error::Validation("file field is required".to_string()));
    };
    let stored = store_status_media(
        &state.config,
        &state.db,
        auth.account_id,
        bytes,
        UploadMeta {
            description,
            focus: parse_focus(focus.as_deref())?,
            declared_content_type: ct,
        },
    )
    .await?;
    let ctx = PostContext::new(state);
    let row = entity::load_media(&ctx, stored.id)
        .await?
        .ok_or_else(|| Error::Internal("media missing after store".to_string()))?;
    Ok(Json(entity::media_entity(&ctx.base_url(), &row)))
}

/// `PUT /api/v1/media/{id}` — update `description`/`focus` (owner
/// only); re-federates the owning status's `Update` when attached.
#[utoipa::path(
    put,
    path = "/api/v1/media/{id}",
    tag = "media",
    params(("id" = String, Path, description = "Media id")),
    request_body(content_type = "application/json", content = MediaUpdateSchema),
    responses(
        (status = 200, description = "The updated media attachment"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:media")?;
    let ctx = PostContext::new(&state);
    let row = entity::load_media(&ctx, id)
        .await?
        .ok_or_else(|| Error::NotFound("media".into()))?;
    if row.account_id != auth.account_id {
        return Err(Error::NotFound("media".into()));
    }
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(request.into_body(), 128 * 1024)
            .await
            .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?,
    )
    .unwrap_or(Value::Null);
    let description = body.get("description").and_then(|v| v.as_str());
    let focus = match body.get("focus") {
        Some(Value::String(s)) => parse_focus(Some(s))?,
        Some(Value::Object(_)) => {
            let x = body["focus"]
                .get("x")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            let y = body["focus"]
                .get("y")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32;
            Some((x, y))
        }
        _ => None,
    };
    if let Some((x, y)) = focus {
        if !(-1.0..=1.0).contains(&x) || !(-1.0..=1.0).contains(&y) {
            return Err(Error::Validation(
                "focus coordinates must be in [-1, 1]".into(),
            ));
        }
        sqlx::query("UPDATE media_attachments SET focus_x = $2, focus_y = $3, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(x)
            .bind(y)
            .execute(&ctx.db)
            .await?;
    }
    if let Some(desc) = description {
        if desc.chars().count() > 5000 {
            return Err(Error::Validation(
                "description must be at most 5000 characters".into(),
            ));
        }
        sqlx::query(
            "UPDATE media_attachments SET description = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(desc)
        .execute(&ctx.db)
        .await?;
    }
    // Re-federate the owning status's Update when attached.
    if let Some(status_id) = row.status_id {
        if let Some(ap) = &ctx.ap {
            if let Ok(Some(status)) = posts::load(&ctx.db, status_id).await {
                if status.local {
                    let mentions = Vec::new();
                    let _ = crate::activitypub::posts::publish_update(ap, &ctx, &status, &mentions)
                        .await;
                    crate::activitypub::posts::invalidate_object(ap, status_id);
                }
            }
        }
    }
    let row = entity::load_media(&ctx, id)
        .await?
        .ok_or_else(|| Error::Internal("media missing after update".to_string()))?;
    Ok(Json(entity::media_entity(&ctx.base_url(), &row)))
}

/// OpenAPI shape of the media update body.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct MediaUpdateSchema {
    /// Alt text.
    pub description: Option<String>,
    /// Focal point `{x, y}` in `[-1, 1]`.
    pub focus: Option<FocusSchema>,
}

/// `focus` object shape.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FocusSchema {
    pub x: f32,
    pub y: f32,
}
