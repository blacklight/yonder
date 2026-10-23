//! `/api/v1` block/mute endpoints — per-account blocks and mutes plus
//! per-user domain blocks (epic 14). Writes federate `Block`/
//! `Undo(Block)`; listings are paginated Account entities.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, paginate, AppState},
    social::{blocks, entity, follows},
    Error, Result,
};

use super::relationships::{account_entities, body_params, param};

/// The `domain` param — body wins, then the query string.
async fn domain_param(request: Request, params: &HashMap<String, String>) -> Result<String> {
    let body = body_params(request).await?;
    param(&body, "domain")
        .or_else(|| params.get("domain").map(String::as_str))
        .map(str::to_string)
        .ok_or_else(|| Error::Validation("domain is required".into()))
}

/// `POST /api/v1/accounts/{id}/block`.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/block",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn block(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:blocks")?;
    follows::visible_target(&state.db, id).await?;
    blocks::block(&state, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unblock` (and `DELETE …/block`).
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/unblock",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn unblock(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:blocks")?;
    blocks::unblock(&state, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/accounts/{id}/mute` — `{notifications?, duration?}`
/// (`duration` is seconds until the mute expires).
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/mute",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn mute(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:mutes")?;
    let params = body_params(request).await?;
    let notifications = super::relationships::bool_param(&params, "notifications").unwrap_or(true);
    // `duration <= 0` (and absent) means indefinite — Mastodon's rule.
    let duration = param(&params, "duration")
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|d| *d > 0);
    follows::visible_target(&state.db, id).await?;
    blocks::mute(&state, auth.account_id, id, notifications, duration).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unmute` (and `DELETE …/mute`).
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/unmute",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn unmute(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:mutes")?;
    blocks::unmute(&state, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `GET /api/v1/blocks` — accounts the caller blocks.
#[utoipa::path(
    get,
    path = "/api/v1/blocks",
    tag = "accounts",
    responses(
        (status = 200, description = "Blocked accounts (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn blocked(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config, &auth, "read:blocks")?;
    let page = paginate::parse(&params);
    let rows = blocks::blocked_list(&state.db, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `GET /api/v1/mutes` — accounts the caller mutes.
#[utoipa::path(
    get,
    path = "/api/v1/mutes",
    tag = "accounts",
    responses(
        (status = 200, description = "Muted accounts (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn muted(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config, &auth, "read:mutes")?;
    let page = paginate::parse(&params);
    let rows = blocks::muted_list(&state.db, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `GET /api/v1/domain_blocks` — the caller's blocked domains.
#[utoipa::path(
    get,
    path = "/api/v1/domain_blocks",
    tag = "accounts",
    responses(
        (status = 200, description = "Blocked domains (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn domain_blocks(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config, &auth, "read:blocks")?;
    let page = paginate::parse(&params);
    let rows = blocks::domain_list(&state.db, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let domains: Vec<String> = rows.into_iter().map(|(d, _)| d).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(domains))))
}

/// `POST /api/v1/domain_blocks` — `domain` param (body or query).
#[utoipa::path(
    post,
    path = "/api/v1/domain_blocks",
    tag = "accounts",
    responses(
        (status = 200, description = "Domain blocked"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 422, description = "Missing domain", body = crate::error::ErrorBody),
    )
)]
pub async fn block_domain(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:blocks")?;
    let domain = domain_param(request, &params).await?;
    blocks::block_domain(&state, auth.account_id, &domain).await?;
    Ok(Json(json!({})))
}

/// `DELETE /api/v1/domain_blocks` — `domain` param (body or query).
#[utoipa::path(
    delete,
    path = "/api/v1/domain_blocks",
    tag = "accounts",
    responses(
        (status = 200, description = "Domain unblocked"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unblock_domain(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:blocks")?;
    let domain = domain_param(request, &params).await?;
    blocks::unblock_domain(&state, auth.account_id, &domain).await?;
    Ok(Json(json!({})))
}
