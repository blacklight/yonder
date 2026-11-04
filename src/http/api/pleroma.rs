//! Akkoma/Pleroma-compatible adapters for Yonder's account-lifecycle
//! APIs — exact upstream route shapes delegating to the same service
//! functions the `yonder` endpoints use. These are Pleroma-family
//! routes, not claimed Mastodon REST surface.
//!
//! Covered contracts (Akkoma `util_controller` / `backup` handlers):
//!
//! - `POST /api/pleroma/move_account` — `{password, target_account}`
//! - `GET/PUT/DELETE /api/pleroma/aliases` — mutations take `{alias}`;
//!   every response returns `{"aliases": [handles]}` like upstream.
//! - `POST /api/pleroma/{follow_import,blocks_import,mutes_import}` —
//!   `{list}` (one acct per line) queues the same archive-import job
//!   the CSV upload uses.
//! - `GET/POST /api/v1/pleroma/backups` — Akkoma's backup index/create
//!   behind `read:backups`; `url` is a short-lived signed download
//!   link served by `…/backups/{id}/download`.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query},
    http::Request,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::scopes::require_scope,
    http::{
        api::v1::transfer::{
            archive_download_response, cooldown_response, queue_archive_export, queue_import,
            ArchiveQueue,
        },
        middleware::{auth::AuthContext, ratelimit},
        AppState,
    },
    users::{moves, transfer, transfer::crypto},
    Error, Result,
};

/// Signed backup-download links live this long after minting — enough
/// for a client to list then fetch, short enough to be throwaway.
const BACKUP_URL_TTL_SECS: i64 = 3600;

/// Mounted under `/api` in `http::api::router`.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        // `move_account` re-verifies the password — a brute-force
        // surface, so it rides the tight `account_move` bucket.
        .route(
            "/api/pleroma/move_account",
            post(move_account).route_layer(ratelimit::layer(state, "account_move")),
        )
        .route(
            "/api/pleroma/aliases",
            get(list_aliases).put(add_alias).delete(remove_alias),
        )
        .route(
            "/api/pleroma/follow_import",
            post(|s, a, r| list_import(s, a, r, "following")),
        )
        .route(
            "/api/pleroma/blocks_import",
            post(|s, a, r| list_import(s, a, r, "blocking")),
        )
        .route(
            "/api/pleroma/mutes_import",
            post(|s, a, r| list_import(s, a, r, "muting")),
        )
        .route(
            "/api/v1/pleroma/backups",
            get(list_backups).post(create_backup),
        )
        .route(
            "/api/v1/pleroma/backups/{id}/download",
            get(download_backup),
        )
}

/// JSON or `application/x-www-form-urlencoded` — Akkoma accepts both.
async fn body_params(req: Request<Body>) -> Result<Value> {
    let (parts, body) = req.into_parts();
    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("body read failed: {e}")))?;
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let pairs: Vec<(String, String)> =
            url::form_urlencoded::parse(&bytes).into_owned().collect();
        let mut map = serde_json::Map::new();
        for (k, v) in pairs {
            map.insert(k, Value::String(v));
        }
        return Ok(Value::Object(map));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Validation(format!("malformed JSON body: {e}")))
}

fn param<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Validation(format!("{name} is required")))
}

/// `POST /api/pleroma/move_account` — password-confirmed move to
/// `target_account`, same validation as `POST /api/v1/yonder/move`.
async fn move_account(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
    req: Request<Body>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let params = body_params(req).await?;
    moves::move_to(
        &state,
        auth.account_id,
        param(&params, "target_account")?,
        param(&params, "password")?,
    )
    .await?;
    Ok(Json(json!({ "status": "success" })))
}

async fn aliases_json(state: &AppState, account_id: i64) -> Result<Json<Value>> {
    Ok(Json(
        json!({ "aliases": moves::list_aliases(&state.db, account_id).await? }),
    ))
}

/// `GET /api/pleroma/aliases` — the caller's `alsoKnownAs` URIs.
async fn list_aliases(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    aliases_json(&state, auth.account_id).await
}

/// `PUT /api/pleroma/aliases` — `{alias}`; idempotent. Upstream
/// returns `{"status": "success"}` on mutations (only `GET` lists).
async fn add_alias(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
    req: Request<Body>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let params = body_params(req).await?;
    moves::add_alias(&state, auth.account_id, param(&params, "alias")?).await?;
    Ok(Json(json!({ "status": "success" })))
}

/// `DELETE /api/pleroma/aliases` — `{alias}`.
async fn remove_alias(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
    req: Request<Body>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let params = body_params(req).await?;
    moves::remove_alias(&state, auth.account_id, param(&params, "alias")?).await?;
    Ok(Json(json!({ "status": "success" })))
}

/// `POST /api/pleroma/{follow,blocks,mutes}_import` — `{list}` is one
/// account address per line, imported through the same job as the CSV
/// upload (headerless single-column rows).
async fn list_import(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
    req: Request<Body>,
    kind_str: &'static str,
) -> Result<impl IntoResponse> {
    let kind = transfer::Kind::parse(kind_str)?;
    require_scope(&state.config(), &auth, kind.write_scope())?;
    let params = body_params(req).await?;
    let list = param(&params, "list")?;
    let bytes = list.as_bytes().to_vec();
    let total = transfer::count_rows(kind, &bytes)?;
    queue_import(
        &state,
        auth.account_id,
        kind,
        "merge",
        &bytes,
        "text/csv",
        total,
        serde_json::json!({}),
    )
    .await
}

/// `GET /api/v1/pleroma/backups` — the caller's archive exports in
/// Akkoma's `{id, content_type, url, file_size, processed,
/// inserted_at}` projection. `url` is a short-lived signed download
/// link: directly fetchable like upstream's public storage URL, but
/// minted only into the owner's own list and dead within the hour.
async fn list_backups(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:backups")?;
    backups_json(&state, auth.account_id).await
}

/// `POST /api/v1/pleroma/backups` — queue an archive and render the
/// refreshed index like upstream; the cooldown is the same 7-day one
/// the Yonder route enforces (Akkoma's `limit_days` equivalent).
async fn create_backup(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Response> {
    require_scope(&state.config(), &auth, "read:backups")?;
    match queue_archive_export(&state, auth.account_id).await? {
        ArchiveQueue::Cooldown(retry) => Ok(cooldown_response(retry)),
        ArchiveQueue::Queued(_) => Ok(backups_json(&state, auth.account_id).await?.into_response()),
    }
}

async fn backups_json(state: &AppState, account_id: i64) -> Result<Json<Value>> {
    let rows = sqlx::query(
        "SELECT * FROM archive_exports WHERE account_id = $1 AND kind = 'archive' \
         AND state <> 'expired' ORDER BY id DESC LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let backups = rows
        .iter()
        .map(|row| backup_json(state, row))
        .collect::<Result<Vec<_>>>()?;
    Ok(Json(Value::Array(backups)))
}

fn backup_json(state: &AppState, row: &sqlx::postgres::PgRow) -> Result<Value> {
    let id: i64 = row.get("id");
    let expires = (chrono::Utc::now() + chrono::Duration::seconds(BACKUP_URL_TTL_SECS)).timestamp();
    let signature = crypto::sign_backup_download(state.config().as_ref(), id, expires)?;
    Ok(json!({
        "id": id.to_string(),
        "content_type": "application/zip",
        "url": format!(
            "{}/api/v1/pleroma/backups/{id}/download?expires={expires}&signature={signature}",
            crate::activitypub::federation_base_url(state.config().as_ref()),
        ),
        "file_size": row.get::<Option<i64>, _>("file_size").unwrap_or(0),
        "processed": row.get::<String, _>("state") == "done",
        "inserted_at": row
            .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    }))
}

#[derive(Deserialize)]
struct BackupDownloadParams {
    expires: i64,
    signature: String,
}

/// `GET /api/v1/pleroma/backups/{id}/download` — the signed link's
/// target. No bearer: the HMAC-bound `{id, expires}` pair is the
/// credential (the URL only ever reaches the export owner's list).
async fn download_backup(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(params): Query<BackupDownloadParams>,
) -> Result<Response> {
    if params.expires <= chrono::Utc::now().timestamp() {
        return Err(Error::Forbidden("download link has expired"));
    }
    if !crypto::verify_backup_download(
        state.config().as_ref(),
        id,
        params.expires,
        &params.signature,
    ) {
        return Err(Error::Forbidden("invalid download signature"));
    }
    // A signed link outlives the account checks the bearer route gets
    // from token resolution, so the owner is re-checked here: a
    // suspended account keeps export access (same as the restricted
    // surface) but disabled, deleted and pending-deletion owners are
    // refused, mirroring `resolve_token`.
    let row = sqlx::query(
        "SELECT e.state, e.file_key, e.expires_at, \
                a.disabled_at IS NOT NULL OR a.deleted_at IS NOT NULL AS gone, \
                EXISTS(SELECT 1 FROM account_deletions d WHERE d.account_id = a.id \
                       AND d.state IN ('pending','running')) AS deleting \
         FROM archive_exports e JOIN accounts a ON a.id = e.account_id \
         WHERE e.id = $1 AND e.kind = 'archive'",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("export".into()))?;
    if row.get::<bool, _>("gone") || row.get::<bool, _>("deleting") {
        return Err(Error::Forbidden("account is no longer available"));
    }
    archive_download_response(&state, &row).await
}
