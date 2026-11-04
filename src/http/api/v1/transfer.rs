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
    http::{
        middleware::auth::{AuthContext, ExportAuth},
        AppState,
    },
    jobs::JobKind,
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
pub(crate) fn status_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<i64, _>("id").to_string(),
        "kind": row.get::<String, _>("kind"),
        "state": row.get::<String, _>("state"),
        "source_type": row.try_get::<String, _>("source_type").unwrap_or_else(|_| "archive".to_string()),
        "source_origin": row.try_get::<Option<String>, _>("source_origin").unwrap_or_default(),
        "source_actor_uri": row.try_get::<Option<String>, _>("source_actor_uri").unwrap_or_default(),
        "stage": row.try_get::<Option<String>, _>("stage").unwrap_or_default(),
        "processed": row.try_get::<i32, _>("processed").unwrap_or_default(),
        "total": row.try_get::<Option<i32>, _>("total").unwrap_or_default(),
        "failed": row.try_get::<i32, _>("failed").unwrap_or_default(),
        "file_size": row.try_get::<Option<i64>, _>("file_size").unwrap_or_default(),
        "error": row.try_get::<Option<String>, _>("error").unwrap_or_default(),
        "options": options_json(row),
        "category_state": summarize_categories(
            &row.try_get::<Value, _>("category_state").unwrap_or_else(|_| json!({})),
        ),
        "summary": row.try_get::<Option<Value>, _>("summary").unwrap_or_default(),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
        "completed_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").unwrap_or_default().map(|t| t.to_rfc3339()),
        "expires_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at").unwrap_or_default().map(|t| t.to_rfc3339()),
    })
}

/// Frozen session options for status responses. `connection_id` is a
/// snowflake, so it travels as a decimal string (JS precision).
fn options_json(row: &sqlx::postgres::PgRow) -> Value {
    let mut options = row
        .try_get::<Value, _>("options")
        .unwrap_or_else(|_| json!({}));
    if let Some(id) = options.get("connection_id").and_then(Value::as_i64) {
        options["connection_id"] = json!(id.to_string());
    }
    options
}

/// Category progress for status responses: scalar counters, flags
/// and notes only — worker internals (page cursors, id maps, the
/// actor cache) can grow to tens of thousands of entries and never
/// belong in a list/status projection.
fn summarize_categories(v: &Value) -> Value {
    let Some(cats) = v.as_object() else {
        return json!({});
    };
    Value::Object(
        cats.iter()
            .map(|(cat, st)| {
                let kept: serde_json::Map<String, Value> = st
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter(|(k, val)| {
                                matches!(val, Value::Bool(_) | Value::Number(_))
                                    || (matches!(k.as_str(), "error" | "note")
                                        && val.as_str().is_some_and(|s| s.len() <= 400))
                            })
                            .map(|(k, val)| (k.clone(), val.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                (cat.clone(), Value::Object(kept))
            })
            .collect(),
    )
}

/// Store `bytes`, insert an `archive_imports` row and enqueue the
/// import job, returning the 202 + status projection shared by the
/// CSV upload and the Pleroma list imports.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn queue_import(
    state: &AppState,
    account_id: i64,
    kind: Kind,
    mode: &str,
    bytes: &[u8],
    content_type: &str,
    total: Option<i32>,
    options: Value,
) -> Result<(StatusCode, Json<Value>)> {
    let key = format!("imports/{}/{}", account_id, uuid::Uuid::new_v4());
    state.media.put(&key, bytes, content_type).await?;
    // `RETURNING *`: no read-back after commit that could fail and invite a duplicate.
    let row = match sqlx::query(
        "INSERT INTO archive_imports (account_id, kind, mode, file_key, total, options) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(account_id)
    .bind(kind.as_str())
    .bind(mode)
    .bind(&key)
    .bind(total)
    .bind(options)
    .fetch_one(&state.db)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            if let Err(cleanup) = state.media.delete(&key).await {
                tracing::warn!(key, error = %cleanup, "orphaned import upload cleanup failed");
            }
            return Err(e.into());
        }
    };
    let import_id: i64 = row.get("id");
    // The sweep recovers a lost envelope; the committed session must not fail the request.
    if let Err(e) = state
        .dispatcher
        .enqueue(JobKind::Import, json!({ "import_id": import_id }))
        .await
    {
        tracing::warn!(import_id, error = %e, "import enqueue failed; relying on sweep");
    }
    Ok((StatusCode::ACCEPTED, Json(status_json(&row))))
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
    auth: ExportAuth,
    Json(params): Json<ExportParams>,
) -> Result<Response> {
    let kind = Kind::parse(&params.kind)?;
    auth.require_scope(&state.config(), "read:accounts")?;
    auth.require_scope(&state.config(), kind.read_scope())?;
    if kind != Kind::Archive {
        let (name, content_type, bytes) = transfer::export_kind(
            &state.db,
            &state.config().instance.domain,
            auth.0.account_id,
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
    match queue_archive_export(&state, auth.0.account_id).await? {
        ArchiveQueue::Cooldown(retry) => Ok(cooldown_response(retry)),
        ArchiveQueue::Queued(row) => {
            Ok((StatusCode::ACCEPTED, Json(status_json(&row))).into_response())
        }
    }
}

/// Outcome of [`queue_archive_export`].
pub(crate) enum ArchiveQueue {
    /// Seconds until the cooldown lifts (see [`cooldown_response`]).
    Cooldown(i64),
    /// The committed export row — already enqueued.
    Queued(sqlx::postgres::PgRow),
}

/// The 429 for an archive request inside the cooldown, with the seconds
/// left as `Retry-After`.
pub(crate) fn cooldown_response(retry: i64) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry.to_string()).unwrap(),
    );
    (
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        Json(json!({"error": "an archive was already requested in the last 7 days"})),
    )
        .into_response()
}

/// Cooldown-guarded archive-export request shared by the Yonder
/// transfer endpoint and the Akkoma backups adapter: one archive per
/// 7 days per account (Mastodon parity; Akkoma has the same idea as a
/// configurable `limit_days`). Failed exports don't count — a broken
/// run must not lock the user out of retries for a week. The
/// per-account advisory lock serializes the check+insert so
/// concurrent requests can't race past the cooldown.
pub(crate) async fn queue_archive_export(
    state: &AppState,
    account_id: i64,
) -> Result<ArchiveQueue> {
    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("export-cooldown:{account_id}"))
        .execute(&mut *tx)
        .await?;
    let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT max(created_at) FROM archive_exports \
         WHERE account_id = $1 AND kind = 'archive' AND state <> 'failed'",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(last) = last {
        let cooldown = chrono::Duration::days(7);
        if chrono::Utc::now() - last < cooldown {
            let retry = (cooldown - (chrono::Utc::now() - last))
                .num_seconds()
                .max(60);
            tx.rollback().await?;
            return Ok(ArchiveQueue::Cooldown(retry));
        }
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO archive_exports (account_id, kind) VALUES ($1, 'archive') RETURNING id",
    )
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    state
        .dispatcher
        .enqueue(JobKind::Export, json!({ "export_id": id }))
        .await?;
    let row = sqlx::query("SELECT * FROM archive_exports WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(ArchiveQueue::Queued(row))
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
    auth: ExportAuth,
) -> Result<Json<Value>> {
    auth.require_scope(&state.config(), "read:accounts")?;
    let rows = sqlx::query(
        "SELECT * FROM archive_exports WHERE account_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(auth.0.account_id)
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
    auth: ExportAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    auth.require_scope(&state.config(), "read:accounts")?;
    let row = sqlx::query("SELECT * FROM archive_exports WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(auth.0.account_id)
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
    auth: ExportAuth,
    Path(id): Path<i64>,
) -> Result<Response> {
    auth.require_scope(&state.config(), "read:accounts")?;
    let row = sqlx::query(
        "SELECT state, file_key, expires_at FROM archive_exports \
         WHERE id = $1 AND account_id = $2",
    )
    .bind(id)
    .bind(auth.0.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("export".into()))?;
    archive_download_response(&state, &row).await
}

/// Stream a `done` archive export row: expired rows are 410, anything
/// not finished is 404, and the zip is served `no-store`. Shared by
/// the bearer-authenticated download and the signed Akkoma-adapter
/// backup URL (the row must already be fetched).
pub(crate) async fn archive_download_response(
    state: &AppState,
    row: &sqlx::postgres::PgRow,
) -> Result<Response> {
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
    let bytes = state.media.get(&key).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"archive.zip\""),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
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
    require_scope(&state.config(), &auth, "write:accounts")?;
    let mut kind = None;
    let mut mode = "merge".to_string();
    let mut federate = false;
    let mut file: Option<Vec<u8>> = None;
    let max = state.config().admin.max_import_size;
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
            "federate" => {
                federate = matches!(
                    field.text().await.unwrap_or_default().as_str(),
                    "true" | "1" | "yes" | "on"
                );
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
    require_scope(&state.config(), &auth, kind.write_scope())?;
    let bytes = file.ok_or_else(|| Error::Validation("file is required".into()))?;
    // Sniff the format before queueing: a parseable ZIP directory or
    // a valid gzip stream (`0x1f 0x8b`, pre-4.2 Mastodon tarballs) for
    // `archive`; anything else must be a plausible text file. A 422
    // here beats an async failure deep in the job.
    if kind == Kind::Archive {
        use std::io::Read;
        let ok = if bytes.starts_with(b"PK\x03\x04") {
            zip::ZipArchive::new(std::io::Cursor::new(&bytes[..])).is_ok()
        } else if bytes.starts_with(b"\x1f\x8b") {
            flate2::read::GzDecoder::new(&bytes[..])
                .take(1 << 20)
                .read_to_end(&mut Vec::new())
                .is_ok()
        } else {
            false
        };
        if !ok {
            return Err(Error::Validation(
                "archive is not a valid zip or tar.gz".into(),
            ));
        }
    }
    // Row-limit check at request time (Mastodon's 20k cap → 422), and
    // `total` counts data rows only — headers excluded.
    let total = transfer::count_rows(kind, &bytes)?;
    // `federate` is the immutable per-session publication choice
    // (contract §8): it only means anything for archives — flat
    // CSV/JSON kinds never create authored posts.
    let options = if kind == Kind::Archive && federate {
        json!({"federate": true})
    } else {
        json!({})
    };
    queue_import(
        &state,
        auth.account_id,
        kind,
        &mode,
        &bytes,
        "application/octet-stream",
        total,
        options,
    )
    .await
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
    require_scope(&state.config(), &auth, "read:accounts")?;
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
    require_scope(&state.config(), &auth, "read:accounts")?;
    let row = sqlx::query("SELECT * FROM archive_imports WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(auth.account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| Error::NotFound("import".into()))?;
    Ok(Json(status_json(&row)))
}

// ---------------------------------------------------------------------------
// Remote OAuth/API import (epic 31 §9) + historical reads (§10)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BeginConnectionForm {
    pub origin: String,
    #[serde(default)]
    pub categories: Vec<String>,
    /// Local path the OAuth callback redirects back to instead of
    /// `/settings/data` (e.g. the migration wizard). Server-validated:
    /// must be a single-segment local path, never a URL.
    #[serde(default)]
    pub return_to: Option<String>,
}

/// `POST /api/v1/yonder/import_connections` — begin OAuth; returns the
/// remote authorization URL the client navigates to.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/import_connections",
    tag = "transfer",
    request_body(content = BeginConnectionForm, description = "Origin + category selection"),
    responses((status = 200, description = "Authorization URL; sets the browser-binding cookie"))
)]
pub async fn begin_connection(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Json(form): axum::extract::Json<BeginConnectionForm>,
) -> Result<([(header::HeaderName, String); 1], Json<Value>)> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let return_to = form
        .return_to
        .as_deref()
        .map(transfer::remote::validate_return_to)
        .transpose()?;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(auth.account_id)
        .fetch_one(&state.db)
        .await?;
    let (url, binding) = transfer::remote::begin_authorization(
        &state,
        user_id,
        &form.origin,
        &form.categories,
        return_to,
    )
    .await?;
    let callback = format!(
        "{}/api/v1/yonder/import_connections/callback",
        crate::activitypub::federation_base_url(state.config().as_ref())
    );
    Ok((
        [(
            header::SET_COOKIE,
            transfer::remote::binding_cookie(&callback, &binding),
        )],
        Json(json!({ "authorization_url": url })),
    ))
}

/// `GET /api/v1/yonder/import_connections` — owned connections.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/import_connections",
    tag = "transfer",
    responses((status = 200, description = "The caller's import connections"))
)]
pub async fn list_connections(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(auth.account_id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(
        transfer::remote::list_connections(&state, user_id).await?,
    ))
}

/// `GET /api/v1/yonder/import_connections/callback` — the remote
/// server's redirect target (no session cookie: single-use state +
/// the browser-binding cookie set at begin).
#[utoipa::path(
    get,
    path = "/api/v1/yonder/import_connections/callback",
    tag = "transfer",
    responses((status = 302, description = "Redirect to the data settings page"))
)]
pub async fn connection_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<([(header::HeaderName, String); 1], axum::response::Redirect)> {
    let code = params.get("code").cloned().unwrap_or_default();
    let st = params.get("state").cloned().unwrap_or_default();
    let binding = transfer::remote::binding_from_cookie(
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()),
    );
    let callback = format!(
        "{}/api/v1/yonder/import_connections/callback",
        crate::activitypub::federation_base_url(state.config().as_ref())
    );
    // The flow is over either way: clear the binding cookie. Errors
    // reach the page as a fixed code — remote transport detail stays
    // in the server log.
    let (outcome, return_to) =
        transfer::remote::complete_authorization(&state, &code, &st, binding.as_deref()).await;
    let dest = return_to.unwrap_or_else(|| "/settings/data".to_string());
    let redirect = match outcome {
        Ok(_) => format!("{dest}?connected=1"),
        Err(e) => {
            let reason = match e {
                Error::Forbidden(_) => "forbidden",
                Error::Validation(_) => "invalid",
                other => {
                    tracing::warn!(error = %other, "remote import authorization failed");
                    "failed"
                }
            };
            format!("{dest}?import_error={reason}")
        }
    };
    Ok((
        [(
            header::SET_COOKIE,
            transfer::remote::expired_binding_cookie(&callback),
        )],
        axum::response::Redirect::to(&redirect),
    ))
}

/// `DELETE /api/v1/yonder/import_connections/{id}` — revoke.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/import_connections/{id}",
    tag = "transfer",
    params(("id" = i64, Path, description = "Connection id")),
    responses((status = 204, description = "Revoked"))
)]
pub async fn revoke_connection(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(auth.account_id)
        .fetch_one(&state.db)
        .await?;
    transfer::remote::revoke_connection(&state, user_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Snowflake ids arrive as decimal strings (JS loses precision past
/// 2^53); accept either representation.
fn de_snowflake<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    match &v {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| serde::de::Error::custom("invalid id")),
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom(
            "id must be a number or decimal string",
        )),
    }
}

fn de_snowflake_opt<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<i64>, D::Error> {
    Option::<Value>::deserialize(d)?
        .map(|v| match &v {
            Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("invalid id")),
            Value::String(s) => s.parse().map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "id must be a number or decimal string",
            )),
        })
        .transpose()
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ImportSessionForm {
    #[serde(deserialize_with = "de_snowflake")]
    #[schema(value_type = String)]
    pub connection_id: i64,
    #[serde(default)]
    pub categories: Vec<String>,
    pub limit: Option<i64>,
    #[serde(default = "default_order")]
    pub order: String,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub federate: bool,
    /// Present remote profile values (even empty ones) replace the
    /// destination's; by default empty values keep them.
    #[serde(default)]
    pub overwrite_profile: bool,
}

fn default_order() -> String {
    "newest".into()
}

/// `POST /api/v1/yonder/import_sessions` — a remote-source session.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/import_sessions",
    tag = "transfer",
    request_body(content = ImportSessionForm, description = "Session options"),
    responses((status = 202, description = "Session created"))
)]
pub async fn create_import_session(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Json(form): axum::extract::Json<ImportSessionForm>,
) -> Result<(StatusCode, Json<Value>)> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let body = transfer::remote::create_session(
        &state,
        auth.account_id,
        transfer::remote::SessionSpec {
            connection_id: form.connection_id,
            categories: &form.categories,
            limit: form.limit,
            order: &form.order,
            since: form.since,
            until: form.until,
            federate: form.federate,
            overwrite_profile: form.overwrite_profile,
        },
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(body)))
}

/// `POST /api/v1/yonder/imports/{id}/cancel`.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/imports/{id}/cancel",
    tag = "transfer",
    params(("id" = i64, Path, description = "Import id")),
    responses((status = 204, description = "Cancellation requested"))
)]
pub async fn cancel_import(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    transfer::remote::cancel_import(&state, auth.account_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ResumeImportForm {
    #[serde(default, deserialize_with = "de_snowflake_opt")]
    #[schema(value_type = Option<String>)]
    pub connection_id: Option<i64>,
}

/// `POST /api/v1/yonder/imports/{id}/resume` — resume a paused
/// session (after reauthorization or suspension lift).
#[utoipa::path(
    post,
    path = "/api/v1/yonder/imports/{id}/resume",
    tag = "transfer",
    params(("id" = i64, Path, description = "Import id")),
    request_body(content = Option<ResumeImportForm>),
    responses((status = 204, description = "Resumed"))
)]
pub async fn resume_import(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    body: Option<axum::extract::Json<ResumeImportForm>>,
) -> Result<StatusCode> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let connection_id = body.and_then(|b| b.connection_id);
    transfer::remote::resume_import(&state, auth.account_id, id, connection_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/yonder/statuses/{id}/history` — owner-scoped
/// historical observations for one imported copy.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/statuses/{id}/history",
    tag = "transfer",
    params(("id" = i64, Path, description = "Status id")),
    responses((status = 200, description = "Historical observations"))
)]
pub async fn status_history(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    Ok(Json(
        transfer::remote::status_history(&state, auth.account_id, id).await?,
    ))
}

/// `GET /api/v1/yonder/history/messages` — owner-only imported
/// private history.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/history/messages",
    tag = "transfer",
    params(("max_id" = Option<i64>, Query, description = "Page before this id")),
    responses((status = 200, description = "Owner-only history"))
)]
pub async fn history_messages(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let max_id = params.get("max_id").and_then(|v| v.parse::<i64>().ok());
    Ok(Json(
        transfer::remote::history_messages(&state, auth.account_id, max_id).await?,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/yonder/history/messages/{id}/media/{index}",
    tag = "transfer",
    params(("id" = i64, Path), ("index" = i32, Path)),
    responses((status = 200, description = "Owner-only attachment download"),
              (status = 404, description = "Not found"))
)]
pub async fn history_media(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path((id, index)): Path<(i64, i32)>,
) -> Result<Response> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let bytes = transfer::remote::history_media(&state, auth.account_id, id, index).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
