//! `/api/v1/tags/*` and `/api/v1/followed_tags` — hashtag lookup and
//! followed-tag management (epic 20). Tag follow is keyed by name
//! (case-insensitive), matching Mastodon.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        paginate, AppState,
    },
    tags::{self, entity::tag_json},
    Error, Result,
};

/// `GET /api/v1/tags/{name}` — public or authenticated. `following`
/// is viewer-specific; `history` carries the last 7 days.
#[utoipa::path(
    get,
    path = "/api/v1/tags/{name}",
    tag = "tags",
    params(("name" = String, Path, description = "Tag name without #")),
    responses(
        (status = 200, description = "Tag entity"),
        (status = 404, description = "No such tag", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid tag name", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let info = tags::lookup(
        &state.db,
        viewer,
        &name,
        state.config.timelines.trends_include_remote,
    )
    .await?
    .ok_or_else(|| Error::NotFound("tag".into()))?;
    let ctx = crate::posts::PostContext::new(&state);
    Ok(Json(tag_json(&ctx.base_url(), &info)))
}

/// `POST /api/v1/tags/{name}/follow` — idempotent. Posts carrying the
/// tag enter the viewer's home feed (public posts only).
#[utoipa::path(
    post,
    path = "/api/v1/tags/{name}/follow",
    tag = "tags",
    params(("name" = String, Path, description = "Tag name without #")),
    responses(
        (status = 200, description = "Tag entity, now followed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
    )
)]
pub async fn follow(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    let info = tags::follow::follow(
        &state.db,
        auth.account_id,
        &name,
        state.config.timelines.trends_include_remote,
    )
    .await?;
    let ctx = crate::posts::PostContext::new(&state);
    Ok(Json(tag_json(&ctx.base_url(), &info)))
}

/// `POST /api/v1/tags/{name}/unfollow` — idempotent.
#[utoipa::path(
    post,
    path = "/api/v1/tags/{name}/unfollow",
    tag = "tags",
    params(("name" = String, Path, description = "Tag name without #")),
    responses(
        (status = 200, description = "Tag entity, no longer followed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No such tag", body = crate::error::ErrorBody),
    )
)]
pub async fn unfollow(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    let info = tags::follow::unfollow(
        &state.db,
        auth.account_id,
        &name,
        state.config.timelines.trends_include_remote,
    )
    .await?
    .ok_or_else(|| Error::NotFound("tag".into()))?;
    let ctx = crate::posts::PostContext::new(&state);
    Ok(Json(tag_json(&ctx.base_url(), &info)))
}

/// `GET /api/v1/followed_tags` — the caller's followed tags,
/// newest-first (cursor = `tag_follows.id`).
#[utoipa::path(
    get,
    path = "/api/v1/followed_tags",
    tag = "tags",
    params(
        ("max_id" = Option<String>, Query, description = "Return tags older than this cursor"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of followed tags (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn followed_tags(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config, &auth, "read:follows")?;
    let page = paginate::parse(&params);
    let rows = tags::follow::followed(
        &state.db,
        auth.account_id,
        &page,
        state.config.timelines.trends_include_remote,
    )
    .await?;
    let ctx = crate::posts::PostContext::new(&state);
    let base = ctx.base_url();
    let cursors: Vec<i64> = rows.iter().map(|(_, cursor)| *cursor).collect();
    let out: Vec<Value> = rows.iter().map(|(t, _)| tag_json(&base, t)).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(out))))
}
