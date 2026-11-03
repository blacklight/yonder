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

use std::sync::Arc;

use axum::{
    body::Body,
    http::Request,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{
        api::v1::transfer::queue_import,
        middleware::{auth::AuthContext, ratelimit},
        AppState,
    },
    users::{moves, transfer},
    Error, Result,
};

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
    )
    .await
}
