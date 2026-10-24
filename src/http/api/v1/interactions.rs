//! `/api/v1` interaction endpoints — favourite, reblog, bookmark,
//! pin and Pleroma-style emoji reactions (epic 13).
//! Handlers stay thin: scope check → [`interactions`] service →
//! entity JSON.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap},
    response::Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        paginate, AppState,
    },
    interactions::{bookmark, boost, favourite, pins, reactions},
    posts::{entity, PostContext},
    users::account as account_mapper,
    Error, Result,
};

/// Render a Status entity for the acting account.
async fn status_json(
    ctx: &PostContext,
    status: &crate::posts::StatusRow,
    viewer: i64,
) -> Result<Value> {
    entity::status_entity(ctx, status, Some(viewer)).await
}

/// Account entities for a list of account ids (order preserved;
/// missing/deleted ids are dropped). One emoji-map load serves the
/// whole page via [`account_mapper::account_entities`].
async fn account_entities(ctx: &PostContext, ids: &[i64]) -> Result<Vec<Value>> {
    let base = ctx.base_url();
    let accounts = account_mapper::load_many(&ctx.db, ids).await?;
    account_mapper::account_entities(&ctx.db, &ctx.config, &base, &accounts).await
}

// ---------------------------------------------------------------------------
// Favourites
// ---------------------------------------------------------------------------

/// `POST /api/v1/statuses/{id}/favourite`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/favourite",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status, flagged favourited"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn favourite(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:favourites")?;
    let ctx = PostContext::new(&state);
    let status = favourite::favourite(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `POST /api/v1/statuses/{id}/unfavourite`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/unfavourite",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unfavourite(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:favourites")?;
    let ctx = PostContext::new(&state);
    let status = favourite::unfavourite(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `GET /api/v1/favourites` — the account's liked statuses.
#[utoipa::path(
    get,
    path = "/api/v1/favourites",
    tag = "statuses",
    responses(
        (status = 200, description = "The favourite statuses (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn favourites_list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: axum::http::Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:favourites")?;
    let ctx = PostContext::new(&state);
    let page = paginate::parse(&params);
    let rows = favourite::list(&ctx, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let mut items = Vec::with_capacity(rows.len());
    for (status, _) in rows {
        items.push(status_json(&ctx, &status, auth.account_id).await?);
    }
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(items))))
}

/// `GET /api/v1/statuses/{id}/favourited_by`.
#[utoipa::path(
    get,
    path = "/api/v1/statuses/{id}/favourited_by",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "Accounts that favourited the status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn favourited_by(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    uri: axum::http::Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    // Anonymous viewers may list likers of public statuses, matching
    // `reblogged_by` and Mastodon; `load_visible` applies `can_see`.
    if let Some(a) = &auth.0 {
        require_scope(&state.config(), a, "read:statuses")?;
    }
    let ctx = PostContext::new(&state);
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let status = crate::interactions::load_visible(&ctx, id, viewer).await?;
    let page = paginate::parse(&params);
    let rows = favourite::favourited_by(&ctx, &status, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&ctx, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

// ---------------------------------------------------------------------------
// Reblog
// ---------------------------------------------------------------------------

/// `POST /api/v1/statuses/{id}/reblog` — optional `visibility`
/// (`public|unlisted`, default `public`), accepted in the body
/// (JSON or form) or the query string.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/reblog",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The boost status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
        (status = 422, description = "The status cannot be boosted", body = crate::error::ErrorBody),
    )
)]
pub async fn reblog(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let visibility = reblog_visibility(&params, request).await?;
    let status = boost::reblog(&ctx, &auth, id, visibility.as_deref()).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `visibility` reaches us via the query string or the body (JSON or
/// form) — Mastodon clients use both. The query string wins.
async fn reblog_visibility(
    params: &HashMap<String, String>,
    request: Request,
) -> Result<Option<String>> {
    if let Some(v) = params.get("visibility") {
        return Ok(Some(v.clone()));
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if content_type.starts_with("application/x-www-form-urlencoded") {
        Ok(url::form_urlencoded::parse(&bytes)
            .find(|(k, _)| k == "visibility")
            .map(|(_, v)| v.into_owned()))
    } else {
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(body
            .get("visibility")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }
}

/// `POST /api/v1/statuses/{id}/unreblog`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/unreblog",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The original status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unreblog(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let status = boost::unreblog(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `GET /api/v1/statuses/{id}/reblogged_by`.
#[utoipa::path(
    get,
    path = "/api/v1/statuses/{id}/reblogged_by",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "Accounts that boosted the status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn reblogged_by(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    uri: axum::http::Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config(), a, "read:statuses")?;
    }
    let ctx = PostContext::new(&state);
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let status = crate::interactions::load_visible(&ctx, id, viewer).await?;
    let page = paginate::parse(&params);
    let rows = boost::reblogged_by(&ctx, &status, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&ctx, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

// ---------------------------------------------------------------------------
// Bookmarks
// ---------------------------------------------------------------------------

/// `POST /api/v1/statuses/{id}/bookmark` — private, no notification.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/bookmark",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status, flagged bookmarked"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn bookmark(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:bookmarks")?;
    let ctx = PostContext::new(&state);
    let status = bookmark::bookmark(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `POST /api/v1/statuses/{id}/unbookmark`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/unbookmark",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unbookmark(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:bookmarks")?;
    let ctx = PostContext::new(&state);
    let status = bookmark::unbookmark(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `GET /api/v1/bookmarks`.
#[utoipa::path(
    get,
    path = "/api/v1/bookmarks",
    tag = "statuses",
    responses(
        (status = 200, description = "The bookmarked statuses (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn bookmarks_list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: axum::http::Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:bookmarks")?;
    let ctx = PostContext::new(&state);
    let page = paginate::parse(&params);
    let rows = bookmark::list(&ctx, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let mut items = Vec::with_capacity(rows.len());
    for (status, _) in rows {
        items.push(status_json(&ctx, &status, auth.account_id).await?);
    }
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(items))))
}

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

/// `POST /api/v1/statuses/{id}/pin` — own statuses only;
/// `content.max_pinned_posts` caps the count.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/pin",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status, flagged pinned"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Not the status author", body = crate::error::ErrorBody),
        (status = 422, description = "Pin cap reached or unpinnable", body = crate::error::ErrorBody),
    )
)]
pub async fn pin(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let ctx = PostContext::new(&state);
    let status = pins::pin(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `POST /api/v1/statuses/{id}/unpin`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/unpin",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unpin(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let ctx = PostContext::new(&state);
    let status = pins::unpin(&ctx, &auth, id).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

// ---------------------------------------------------------------------------
// Emoji reactions (Pleroma paths)
// ---------------------------------------------------------------------------

/// `PUT|POST /api/v1/pleroma/statuses/{id}/reactions/{emoji}`.
#[utoipa::path(
    put,
    path = "/api/v1/pleroma/statuses/{id}/reactions/{emoji}",
    tag = "statuses",
    params(
        ("id" = String, Path, description = "Status id"),
        ("emoji" = String, Path, description = "Unicode emoji or :shortcode:"),
    ),
    responses(
        (status = 200, description = "The status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 422, description = "Unknown or invalid emoji", body = crate::error::ErrorBody),
    )
)]
pub async fn react(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path((id, emoji)): Path<(i64, String)>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let status = reactions::react(&ctx, &auth, id, &emoji).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `DELETE /api/v1/pleroma/statuses/{id}/reactions/{emoji}`.
#[utoipa::path(
    delete,
    path = "/api/v1/pleroma/statuses/{id}/reactions/{emoji}",
    tag = "statuses",
    params(
        ("id" = String, Path, description = "Status id"),
        ("emoji" = String, Path, description = "Unicode emoji or :shortcode:"),
    ),
    responses(
        (status = 200, description = "The status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unreact(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path((id, emoji)): Path<(i64, String)>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let status = reactions::unreact(&ctx, &auth, id, &emoji).await?;
    Ok(Json(status_json(&ctx, &status, auth.account_id).await?))
}

/// `GET /api/v1/pleroma/statuses/{id}/reactions` — the per-emoji
/// reaction summary.
#[utoipa::path(
    get,
    path = "/api/v1/pleroma/statuses/{id}/reactions",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "Per-emoji reaction counts"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn reactions_list(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let ctx = PostContext::new(&state);
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let status = crate::interactions::load_visible(&ctx, id, viewer).await?;
    Ok(Json(Value::Array(
        reactions::reaction_summary(&ctx, status.id, viewer, true).await?,
    )))
}
