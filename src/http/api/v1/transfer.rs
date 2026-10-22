//! `/api/v1/yonder/imports` and `/api/v1/yonder/exports` (epic 11):
//! account-archive transfer endpoints. CSV/JSON exports are generated
//! inline; `archive` exports and every import run through the job
//! queue with progress on `archive_exports`/`archive_imports`.

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    jobs::JobKind,
    media::storage,
    users::transfer::{self, Kind},
    Error, Result,
};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ExportParams {
    /// `following` | `followers` | `blocking` | `muting` |
    /// `domain_blocks` | `bookmarks` | `lists` | `filters` | `archive`.
    pub kind: String,
}

/// An `archive_exports`/`archive_imports` status projection.
fn status_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<i64, _>("id").to_string(),
        "kind": row.get::<String, _>("kind"),
        "state": row.get::<String, _>("state"),
        "processed": row.try_get::<i32, _>("processed").unwrap_or_default(),
        "total": row.try_get::<Option<i32>, _>("total").unwrap_or_default(),
        "failed": row.try_get::<i32, _>("failed").unwrap_or_default(),
        "file_size": row.try_get::<Option<i64>, _>("file_size").unwrap_or_default(),
        "error": row.try_get::<Option<String>, _>("error").unwrap_or_default(),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
        "completed_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").unwrap_or_default().map(|t| t.to_rfc3339()),
        "expires_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at").unwrap_or_default().map(|t| t.to_rfc3339()),
    })
}

/// `POST /api/v1/yonder/exports` — CSV/JSON kinds download inline;
/// `archive` returns 202 with the queued export row.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/exports",
    tag = "transfer",
    request_body = ExportParams,
    responses(
        (status = 200, description = "Inline CSV/JSON export"),
        (status = 202, description = "Archive export queued"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 429, description = "Archive cooldown (one per 7 days)", body = crate::error::ErrorBody),
    )
)]
pub async fn create_export(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(params): Json<ExportParams>,
) -> Result<Response> {
    let kind = Kind::parse(&params.kind)?;
    require_scope(&state.config, &auth, "read:accounts")?;
    require_scope(&state.config, &auth, kind.read_scope())?;
    if kind != Kind::Archive {
        let (name, content_type, bytes) = transfer::export_kind(
            &state.db,
            &state.config.instance.domain,
            auth.account_id,
            kind,
        )
        .await?;
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
        return Ok((StatusCode::OK, headers, bytes).into_response());
    }
    // Archive cooldown: one per 7 days per account (Mastodon).
    // Failed exports don't count — a broken run must not lock the user
    // out of retries for a week.
    let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT max(created_at) FROM archive_exports \
         WHERE account_id = $1 AND kind = 'archive' AND state <> 'failed'",
    )
    .bind(auth.account_id)
    .fetch_one(&state.db)
    .await?;
    if let Some(last) = last {
        let cooldown = chrono::Duration::days(7);
        if chrono::Utc::now() - last < cooldown {
            let retry = (cooldown - (chrono::Utc::now() - last))
                .num_seconds()
                .max(60);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&retry.to_string()).unwrap(),
            );
            return Ok((
                StatusCode::TOO_MANY_REQUESTS,
                headers,
                Json(json!({"error": "an archive was already requested in the last 7 days"})),
            )
                .into_response());
        }
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind) VALUES ($1, 'archive') RETURNING id",
    )
    .bind(auth.account_id)
    .fetch_one(&state.db)
    .await?;
    state
        .dispatcher
        .enqueue(JobKind::Export, json!({ "export_id": id }))
        .await?;
    let row = sqlx::query("SELECT * FROM archive_exports WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(status_json(&row))).into_response())
}

/// `GET /api/v1/yonder/exports` — list own archive exports.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/exports",
    tag = "transfer",
    responses((status = 200, description = "The caller's archive exports"))
)]
pub async fn list_exports(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:accounts")?;
    let rows = sqlx::query(
        "SELECT * FROM archive_exports WHERE account_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(auth.account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Value::Array(rows.iter().map(status_json).collect())))
}

/// `GET /api/v1/yonder/exports/{id}` — one export's status.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/exports/{id}",
    tag = "transfer",
    params(("id" = i64, Path, description = "Export id")),
    responses(
        (status = 200, description = "Export status"),
        (status = 404, description = "Unknown export", body = crate::error::ErrorBody),
    )
)]
pub async fn get_export(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:accounts")?;
    let row = sqlx::query("SELECT * FROM archive_exports WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(auth.account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| Error::NotFound("export".into()))?;
    Ok(Json(status_json(&row)))
}

/// `GET /api/v1/yonder/exports/{id}/download` — stream the zip
/// (owner only; 410 once expired).
#[utoipa::path(
    get,
    path = "/api/v1/yonder/exports/{id}/download",
    tag = "transfer",
    params(("id" = i64, Path, description = "Export id")),
    responses(
        (status = 200, description = "The archive zip"),
        (status = 404, description = "Unknown or unfinished export", body = crate::error::ErrorBody),
        (status = 410, description = "Export expired", body = crate::error::ErrorBody),
    )
)]
pub async fn download_export(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Response> {
    require_scope(&state.config, &auth, "read:accounts")?;
    let row = sqlx::query(
        "SELECT state, file_key, expires_at FROM archive_exports \
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(auth.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("export".into()))?;
    let state_col: String = row.get("state");
    if state_col == "expired" {
        return Ok((
            StatusCode::GONE,
            Json(json!({ "error": "export has expired" })),
        )
            .into_response());
    }
    if state_col != "done" {
        return Err(Error::NotFound("export".into()));
    }
    if let Some(expires) = row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at") {
        if expires < chrono::Utc::now() {
            return Ok((
                StatusCode::GONE,
                Json(json!({ "error": "export has expired" })),
            )
                .into_response());
        }
    }
    let key: Option<String> = row.get("file_key");
    let Some(key) = key else {
        return Err(Error::NotFound("export".into()));
    };
    let bytes = storage::read_bytes(&state.config, &key).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"archive.zip\""),
    );
    Ok((StatusCode::OK, headers, bytes).into_response())
}

/// `POST /api/v1/yonder/imports` — multipart: `kind`, `mode`, `file`.
/// Validates and stores the upload, then queues the `Import` job.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/imports",
    tag = "transfer",
    responses(
        (status = 202, description = "Import queued"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create_import(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Value>)> {
    require_scope(&state.config, &auth, "write:accounts")?;
    let mut kind = None;
    let mut mode = "merge".to_string();
    let mut file: Option<Vec<u8>> = None;
    let max = state.config.admin.max_import_size;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart: {e}")))?
    {
        match field.name().unwrap_or("") {
            "type" | "kind" => kind = Some(field.text().await.unwrap_or_default()),
            "mode" => {
                let m = field.text().await.unwrap_or_default();
                if matches!(m.as_str(), "merge" | "overwrite") {
                    mode = m;
                } else {
                    return Err(Error::Validation("mode must be merge or overwrite".into()));
                }
            }
            "data" | "file" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| Error::Validation(format!("file read failed: {e}")))?;
                if bytes.len() as u64 > max {
                    return Err(Error::Validation("import file is too large".into()));
                }
                file = Some(bytes.to_vec());
            }
            _ => {}
        }
    }
    let kind = kind
        .as_deref()
        .map(Kind::parse)
        .transpose()?
        .ok_or_else(|| Error::Validation("kind is required".into()))?;
    if !kind.importable() {
        return Err(Error::Validation(format!(
            "kind {:?} is not importable",
            kind.as_str()
        )));
    }
    require_scope(&state.config, &auth, kind.write_scope())?;
    let bytes = file.ok_or_else(|| Error::Validation("file is required".into()))?;
    // Sniff the format before queueing: a zip for `archive`, anything
    // else must be a plausible text file.
    if kind == Kind::Archive {
        let sniff = zip::ZipArchive::new(std::io::Cursor::new(&bytes[..]))
            .map_err(|e| Error::Validation(format!("archive is not a valid zip: {e}")))?;
        let _ = sniff.len();
    }
    // Row-limit check at request time (Mastodon's 20k cap → 422), and
    // `total` counts data rows only — headers excluded.
    let total = transfer::count_rows(kind, &bytes)?;
    let key = format!("imports/{}/{}", auth.account_id, uuid::Uuid::new_v4());
    storage::write_bytes(&state.config, &key, &bytes).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key, total) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(auth.account_id)
    .bind(kind.as_str())
    .bind(&mode)
    .bind(&key)
    .bind(total)
    .fetch_one(&state.db)
    .await?;
    state
        .dispatcher
        .enqueue(JobKind::Import, json!({ "import_id": id }))
        .await?;
    let row = sqlx::query("SELECT * FROM archive_imports WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(status_json(&row))))
}

/// `GET /api/v1/yonder/imports` — list own imports.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/imports",
    tag = "transfer",
    responses((status = 200, description = "The caller's imports"))
)]
pub async fn list_imports(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:accounts")?;
    let rows = sqlx::query(
        "SELECT * FROM archive_imports WHERE account_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(auth.account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Value::Array(rows.iter().map(status_json).collect())))
}

/// `GET /api/v1/yonder/imports/{id}` — one import's status/progress.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/imports/{id}",
    tag = "transfer",
    params(("id" = i64, Path, description = "Import id")),
    responses(
        (status = 200, description = "Import status"),
        (status = 404, description = "Unknown import", body = crate::error::ErrorBody),
    )
)]
pub async fn get_import(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:accounts")?;
    let row = sqlx::query("SELECT * FROM archive_imports WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(auth.account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| Error::NotFound("import".into()))?;
    Ok(Json(status_json(&row)))
}
