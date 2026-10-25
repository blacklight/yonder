//! `/api/v1/admin/emojis*` and `/api/v1/admin/emoji_packs*` — the
//! emoji administration surface epic 22's UI consumes (epic 20).
//! Every route requires the `admin` role via `require_scope`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    emoji::{entity::emoji_entity, import, model},
    http::{middleware::auth::AuthContext, AppState},
    posts::PostContext,
    Error, Result,
};

/// Shared renderer — one row → entity. The admin surface adds the
/// row `id` (needed by PATCH/DELETE/{id}), `disabled`, and `pack_id`,
/// none of which belong in the public `CustomEmoji` shape.
fn entity_of(state: &Arc<AppState>, row: &model::EmojiRow) -> Value {
    let ctx = PostContext::new(state);
    let mut v = emoji_entity(&state.config(), &ctx.base_url(), row);
    v["id"] = json!(row.id.to_string());
    v["disabled"] = json!(row.disabled);
    v["pack_id"] = match row.pack_id {
        Some(id) => json!(id.to_string()),
        None => Value::Null,
    };
    v
}

/// The per-entry outcome report shared by both import endpoints.
fn import_report(state: &Arc<AppState>, outcomes: &[import::ImportOutcome]) -> Value {
    let ctx = PostContext::new(state);
    let base = ctx.base_url();
    json!({
        "imported": outcomes.iter().filter(|o| o.result.is_ok()).count(),
        "skipped": outcomes.iter().filter(|o| o.result.is_err()).count(),
        "results": outcomes.iter().map(|o| {
            match &o.result {
                Ok(row) => json!({ "name": o.name, "ok": true, "emoji": entity_of(state, row) }),
                Err(msg) => json!({ "name": o.name, "ok": false, "error": msg }),
            }
        }).collect::<Vec<_>>(),
    })
}

/// `POST /api/v1/admin/emojis` — multipart `file` + `shortcode`
/// (+optional `pack_id`, `category`).
#[utoipa::path(
    post,
    path = "/api/v1/admin/emojis",
    tag = "admin",
    request_body(content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "The created CustomEmoji"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 409, description = "Shortcode already taken", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid shortcode or file", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    mut mp: Multipart,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let mut file: Option<Vec<u8>> = None;
    let mut shortcode = None;
    let mut pack_id = None;
    let mut category = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart body: {e}")))?
    {
        match field.name() {
            Some("file") => {
                file = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| Error::Validation(format!("read failed: {e}")))?
                        .to_vec(),
                );
            }
            Some("shortcode") => shortcode = Some(field.text().await.unwrap_or_default()),
            Some("pack_id") => {
                let text = field.text().await.unwrap_or_default();
                pack_id = Some(
                    text.parse::<i64>()
                        .map_err(|_| Error::Validation("pack_id must be an integer".into()))?,
                );
            }
            Some("category") => category = Some(field.text().await.unwrap_or_default()),
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let bytes = file.ok_or_else(|| Error::Validation("file field is required".into()))?;
    let shortcode =
        shortcode.ok_or_else(|| Error::Validation("shortcode field is required".into()))?;
    let row = model::create_local(
        &state.db,
        &state.media,
        &state.config(),
        &shortcode,
        bytes,
        auth.account_id,
        pack_id,
        category.as_deref(),
    )
    .await?;
    Ok(Json(entity_of(&state, &row)))
}

/// `GET /api/v1/admin/emojis` — local rows, optionally pack-filtered.
#[utoipa::path(
    get,
    path = "/api/v1/admin/emojis",
    tag = "admin",
    params(
        ("pack_id" = Option<i64>, Query, description = "Limit to one pack"),
        ("include_disabled" = Option<bool>, Query, description = "Include disabled emoji")
    ),
    responses((status = 200, description = "Array of CustomEmoji entities"))
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let pack_id = params.get("pack_id").and_then(|s| s.parse::<i64>().ok());
    let include_disabled = params
        .get("include_disabled")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let rows = model::list_admin(&state.db, pack_id, include_disabled).await?;
    Ok(Json(Value::Array(
        rows.iter().map(|r| entity_of(&state, r)).collect(),
    )))
}

/// `GET /api/v1/admin/emojis/{id}` — one emoji.
#[utoipa::path(
    get,
    path = "/api/v1/admin/emojis/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Emoji id")),
    responses(
        (status = 200, description = "The CustomEmoji"),
        (status = 404, description = "No such emoji", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    Ok(Json(entity_of(&state, &model::get(&state.db, id).await?)))
}

/// `PATCH /api/v1/admin/emojis/{id}` — rename, recategorize, repack,
/// toggle visibility/disabled. `category`/`pack_id` are tri-state:
/// absent keeps the value, JSON `null` clears it (see
/// [`model::EmojiPatch`]).
#[utoipa::path(
    patch,
    path = "/api/v1/admin/emojis/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Emoji id")),
    request_body = Value,
    responses(
        (status = 200, description = "The updated CustomEmoji"),
        (status = 404, description = "No such emoji", body = crate::error::ErrorBody),
        (status = 409, description = "Shortcode already taken", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<model::EmojiPatch>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let row = model::update(&state.db, id, body).await?;
    Ok(Json(entity_of(&state, &row)))
}

/// `DELETE /api/v1/admin/emojis/{id}` — removes the row and its
/// stored image.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/emojis/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Emoji id")),
    responses(
        (status = 200, description = "Deleted (empty body)"),
        (status = 404, description = "No such emoji", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_emoji(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    model::delete(&state.db, &state.media, id).await?;
    Ok(Json(Value::Object(Default::default())))
}

/// `POST /api/v1/admin/emojis/import_zip` — multipart `file` + optional
/// `pack_id` + `overwrite`. Per-entry outcomes returned as a list.
#[utoipa::path(
    post,
    path = "/api/v1/admin/emojis/import_zip",
    tag = "admin",
    request_body(content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Per-entry import outcomes"),
        (status = 422, description = "Not a zip, or an archive-level cap was exceeded", body = crate::error::ErrorBody),
    )
)]
pub async fn import_zip(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    mut mp: Multipart,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let mut file: Option<Vec<u8>> = None;
    let mut pack_id = None;
    let mut overwrite = false;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart body: {e}")))?
    {
        match field.name() {
            Some("file") => {
                file = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| Error::Validation(format!("read failed: {e}")))?
                        .to_vec(),
                );
            }
            Some("pack_id") => {
                let text = field.text().await.unwrap_or_default();
                pack_id = Some(
                    text.parse::<i64>()
                        .map_err(|_| Error::Validation("pack_id must be an integer".into()))?,
                );
            }
            Some("overwrite") => {
                overwrite = field
                    .text()
                    .await
                    .map(|s| s == "true" || s == "1")
                    .unwrap_or(false)
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let bytes = file.ok_or_else(|| Error::Validation("file field is required".into()))?;
    if let Some(p) = pack_id {
        model::get_pack(&state.db, p).await?;
    }
    let outcomes = import::import_zip(
        &state.db,
        &state.media,
        &state.config(),
        bytes,
        auth.account_id,
        pack_id,
        overwrite,
    )
    .await?;
    Ok(Json(import_report(&state, &outcomes)))
}

/// `POST /api/v1/admin/emojis/import_dir` — import image files from a
/// directory under `admin.emoji_import_dir`.
#[derive(Debug, Deserialize)]
pub struct ImportDirBody {
    pub from_dir: String,
    pub pack_id: Option<i64>,
}

/// `POST /api/v1/admin/emojis/import_dir` — flat, non-recursive scan.
#[utoipa::path(
    post,
    path = "/api/v1/admin/emojis/import_dir",
    tag = "admin",
    request_body = Value,
    responses(
        (status = 200, description = "Per-entry import outcomes"),
        (status = 404, description = "Directory not found", body = crate::error::ErrorBody),
        (status = 422, description = "Import disabled or path escapes root", body = crate::error::ErrorBody),
    )
)]
pub async fn import_dir(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<ImportDirBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    if let Some(p) = body.pack_id {
        model::get_pack(&state.db, p).await?;
    }
    let outcomes = import::from_dir(
        &state.db,
        &state.media,
        &state.config(),
        &body.from_dir,
        auth.account_id,
        body.pack_id,
    )
    .await?;
    Ok(Json(import_report(&state, &outcomes)))
}

/// `GET /api/v1/admin/emoji_packs` — packs with emoji counts.
#[utoipa::path(
    get,
    path = "/api/v1/admin/emoji_packs",
    tag = "admin",
    responses((status = 200, description = "Array of packs with counts"))
)]
pub async fn list_packs(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let packs = model::list_packs(&state.db).await?;
    Ok(Json(json!(packs
        .iter()
        .map(|(p, count)| json!({
            "id": p.id.to_string(),
            "name": p.name,
            "emoji_count": count,
        }))
        .collect::<Vec<_>>())))
}

/// `POST /api/v1/admin/emoji_packs` — create a pack.
#[derive(Debug, Deserialize)]
pub struct PackBody {
    pub name: String,
}

/// `POST /api/v1/admin/emoji_packs` — `{name}` → the pack row.
#[utoipa::path(
    post,
    path = "/api/v1/admin/emoji_packs",
    tag = "admin",
    request_body = Value,
    responses(
        (status = 200, description = "The created pack"),
        (status = 409, description = "Name already taken", body = crate::error::ErrorBody),
    )
)]
pub async fn create_pack(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<PackBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let p = model::create_pack(&state.db, &body.name).await?;
    Ok(Json(json!({ "id": p.id.to_string(), "name": p.name })))
}

/// `PATCH /api/v1/admin/emoji_packs/{id}` — rename.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/emoji_packs/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Pack id")),
    request_body = Value,
    responses(
        (status = 200, description = "The renamed pack"),
        (status = 404, description = "No such pack", body = crate::error::ErrorBody),
    )
)]
pub async fn update_pack(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<PackBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let p = model::update_pack(&state.db, id, &body.name).await?;
    Ok(Json(json!({ "id": p.id.to_string(), "name": p.name })))
}

/// `DELETE /api/v1/admin/emoji_packs/{id}` — emoji keep their rows
/// with `pack_id` cleared.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/emoji_packs/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Pack id")),
    responses(
        (status = 200, description = "Deleted (empty body)"),
        (status = 404, description = "No such pack", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_pack(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    model::delete_pack(&state.db, id).await?;
    Ok(Json(Value::Object(Default::default())))
}
