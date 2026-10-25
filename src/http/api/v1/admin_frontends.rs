//! `/api/v1/yonder/admin/frontends*` — alternative-frontend
//! administration (epic 23): list, install (zip upload, filesystem
//! path or git) and remove — plus the user-facing
//! `GET /api/v1/yonder/frontends` used by the settings picker.
//! Admin mutations write one `audit_log` row each; the domain logic
//! lives in [`crate::frontends`].

use std::sync::Arc;

use axum::{
    body::to_bytes,
    extract::{FromRequest, Multipart, Path, Request, State},
    http::header,
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    frontends::{self, install},
    http::{middleware::auth::AuthContext, AppState},
    moderation::audit,
    Error, Result,
};

/// JSON install bodies are small config documents — a few KB of
/// `{"name", "source", "path"|"url"}` — not archives, so they get a
/// much tighter cap than `frontends.max_upload_size`.
const JSON_INSTALL_BODY_LIMIT: usize = 64 * 1024;

fn frontend_json(fe: &frontends::Frontend, admin: bool) -> Value {
    if admin {
        json!({
            "name": fe.name,
            "version": fe.manifest.version,
            "source": fe.manifest.source,
            "builtin": fe.name == frontends::BUILTIN,
        })
    } else {
        // Never leak filesystem paths or git URLs to regular users.
        json!({ "name": fe.name, "version": fe.manifest.version })
    }
}

fn list_json(state: &AppState, admin: bool) -> Result<Json<Value>> {
    let config = state.config();
    let mut frontends_list: Vec<frontends::Frontend> = vec![frontends::builtin(&config)];
    frontends_list.extend(frontends::list(&config)?);
    Ok(Json(json!({
        "default": config.admin.default_frontend,
        "frontends": frontends_list
            .iter()
            .map(|f| frontend_json(f, admin))
            .collect::<Vec<_>>(),
    })))
}

/// `GET /api/v1/yonder/frontends` — installed frontends a regular
/// user may pick in settings. Read-only, names and versions only.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/frontends",
    tag = "settings",
    responses(
        (status = 200, description = "Available frontends"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list_available(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    list_json(&state, false)
}

/// `GET /api/v1/yonder/admin/frontends` — installed frontends with
/// install source metadata, and the admin default.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/admin/frontends",
    tag = "admin",
    responses(
        (status = 200, description = "Installed frontends"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn list_frontends(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    list_json(&state, true)
}

/// `POST /api/v1/yonder/admin/frontends` — install a frontend.
/// `multipart/form-data` uploads a zip (`file` + `name`, optional
/// `version`); `application/json` installs from a server path or a
/// git repository (`{"name", "source": "path"|"git", ...}`).
#[utoipa::path(
    post,
    path = "/api/v1/yonder/admin/frontends",
    tag = "admin",
    request_body(
        content = String,
        description = "Multipart zip (file + name + version) or JSON {name, source, path|url|ref}",
        content_type = "multipart/form-data"
    ),
    responses(
        (status = 200, description = "Installed frontend"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 409, description = "Frontend name already installed", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn install_frontend(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;

    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/form-data"));

    let installed = if is_multipart {
        let multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|e| Error::Validation(format!("malformed multipart: {e}")))?;
        install_from_multipart(&state, multipart).await?
    } else {
        let body = to_bytes(request.into_body(), JSON_INSTALL_BODY_LIMIT)
            .await
            .map_err(|e| Error::Validation(format!("body read failed: {e}")))?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|e| Error::Validation(format!("invalid JSON body: {e}")))?;
        install_from_json(&state, &value).await?
    };

    audit::record(
        &state.db,
        Some(auth.account_id),
        "frontend.install",
        "frontend",
        None,
        json!({ "name": installed.name }),
    )
    .await?;
    Ok(Json(frontend_json(&installed, true)))
}

/// Multipart zip install: `name`, optional `version`, `file`.
async fn install_from_multipart(
    state: &AppState,
    mut multipart: Multipart,
) -> Result<frontends::Frontend> {
    let mut name = None;
    let mut version = None;
    let mut file: Option<Vec<u8>> = None;
    let max = state.config().frontends.max_upload_size;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart: {e}")))?
    {
        match field.name().unwrap_or("") {
            "name" => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| Error::Validation(format!("name field read failed: {e}")))?,
                )
            }
            "version" => {
                version =
                    Some(field.text().await.map_err(|e| {
                        Error::Validation(format!("version field read failed: {e}"))
                    })?)
            }
            "file" | "data" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| Error::Validation(format!("file read failed: {e}")))?;
                if bytes.len() as u64 > max {
                    return Err(Error::Validation("frontend archive is too large".into()));
                }
                file = Some(bytes.to_vec());
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| Error::Validation("name is required".into()))?;
    let file = file.ok_or_else(|| Error::Validation("a file field is required".into()))?;
    install::install_from_zip(&state.config(), &name, version.as_deref(), &file).await
}

/// JSON install: `{name, source: "path"|"git", version?, path?, url?, ref?}`.
async fn install_from_json(state: &AppState, body: &Value) -> Result<frontends::Frontend> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Validation("name is required".into()))?;
    let version = body.get("version").and_then(Value::as_str);
    match body.get("source").and_then(Value::as_str) {
        Some("path") => {
            let path = body
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Validation("path is required".into()))?;
            install::install_from_path(&state.config(), name, version, path).await
        }
        Some("git") => {
            let url = body
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Validation("url is required".into()))?;
            let git_ref = body.get("ref").and_then(Value::as_str);
            install::install_from_git(&state.config(), name, version, url, git_ref).await
        }
        Some(other) => Err(Error::Validation(format!(
            "source must be \"path\" or \"git\", got {other:?}"
        ))),
        None => Err(Error::Validation(
            "JSON installs require source: \"path\" or \"git\"".into(),
        )),
    }
}

/// `DELETE /api/v1/yonder/admin/frontends/{name}` — remove an
/// installed frontend; the builtin and the admin default refuse.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/admin/frontends/{name}",
    tag = "admin",
    params(("name" = String, Path, description = "Frontend name")),
    responses(
        (status = 200, description = "Removed"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "Frontend not installed", body = crate::error::ErrorBody),
        (status = 422, description = "Builtin or default frontend", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_frontend(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    install::remove(&state.config(), &name).await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "frontend.remove",
        "frontend",
        None,
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(json!({ "name": name, "removed": true })))
}
