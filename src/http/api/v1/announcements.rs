//! `/api/v1/announcements` — Mastodon instance announcements.
//! Yonder has no admin announcement surface yet, so the list is
//! always empty — the response shape stays Mastodon-compatible and
//! mutation endpoints report the nonexistent announcement as 404,
//! which is exactly what Mastodon returns for an unknown id.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{json, Value};

use crate::{
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// `GET /api/v1/announcements?with_dismissed=` — empty list; there
/// are no announcements to deliver.
#[utoipa::path(
    get,
    path = "/api/v1/announcements",
    tag = "announcements",
    params(("with_dismissed" = Option<bool>, Query, description = "Include dismissed announcements")),
    responses((status = 200, description = "Array of Announcement entities (empty — no announcements exist)"))
)]
pub async fn list(State(_state): State<Arc<AppState>>, _auth: AuthContext) -> Result<Json<Value>> {
    Ok(Json(json!([])))
}

fn missing() -> Error {
    Error::NotFound("announcement".into())
}

/// `POST /api/v1/announcements/{id}/dismiss` — 404: the id cannot
/// exist while the list is empty.
#[utoipa::path(
    post,
    path = "/api/v1/announcements/{id}/dismiss",
    tag = "announcements",
    params(("id" = String, Path, description = "Announcement id")),
    responses(
        (status = 200, description = "Dismissed"),
        (status = 404, description = "No such announcement", body = crate::error::ErrorBody),
    )
)]
pub async fn dismiss(
    State(_state): State<Arc<AppState>>,
    _auth: AuthContext,
    Path(_id): Path<i64>,
) -> Result<Json<Value>> {
    Err(missing())
}

/// `PUT /api/v1/announcements/{id}/reactions/{name}` — 404.
#[utoipa::path(
    put,
    path = "/api/v1/announcements/{id}/reactions/{name}",
    tag = "announcements",
    params(
        ("id" = String, Path, description = "Announcement id"),
        ("name" = String, Path, description = "Emoji code")
    ),
    responses(
        (status = 200, description = "Reaction added"),
        (status = 404, description = "No such announcement", body = crate::error::ErrorBody),
    )
)]
pub async fn add_reaction(
    State(_state): State<Arc<AppState>>,
    _auth: AuthContext,
    Path(_path): Path<(i64, String)>,
) -> Result<Json<Value>> {
    Err(missing())
}

/// `DELETE /api/v1/announcements/{id}/reactions/{name}` — 404.
#[utoipa::path(
    delete,
    path = "/api/v1/announcements/{id}/reactions/{name}",
    tag = "announcements",
    params(
        ("id" = String, Path, description = "Announcement id"),
        ("name" = String, Path, description = "Emoji code")
    ),
    responses(
        (status = 200, description = "Reaction removed"),
        (status = 404, description = "No such announcement", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_reaction(
    State(_state): State<Arc<AppState>>,
    _auth: AuthContext,
    Path(_path): Path<(i64, String)>,
) -> Result<Json<Value>> {
    Err(missing())
}
