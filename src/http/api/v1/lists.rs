//! `/api/v1/lists` — list CRUD and membership (epic 14). Ownership is
//! enforced in the service layer; foreign lists are 404.

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
    social::lists,
    Error, Result,
};

use super::relationships::{account_entities, body_params, bool_param, id_list, param};

/// `GET /api/v1/lists` — the caller's lists.
#[utoipa::path(
    get,
    path = "/api/v1/lists",
    tag = "lists",
    responses(
        (status = 200, description = "The caller's lists (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:lists")?;
    let page = paginate::parse(&params);
    let rows = lists::list(&state.db, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|l| l.id).collect();
    let entities: Vec<Value> = rows.iter().map(|l| l.entity()).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(entities))))
}

/// `POST /api/v1/lists` — `{title, replies_policy?, exclusive?}`.
#[utoipa::path(
    post,
    path = "/api/v1/lists",
    tag = "lists",
    responses(
        (status = 200, description = "The created list"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid title or replies_policy", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:lists")?;
    let params = body_params(request).await?;
    let title =
        param(&params, "title").ok_or_else(|| Error::Validation("title is required".into()))?;
    let policy = param(&params, "replies_policy").unwrap_or("list");
    let exclusive = bool_param(&params, "exclusive").unwrap_or(false);
    let list = lists::create(&state.db, auth.account_id, title, policy, exclusive).await?;
    Ok(Json(list.entity()))
}

/// `GET /api/v1/lists/{id}`.
#[utoipa::path(
    get,
    path = "/api/v1/lists/{id}",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "The list"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:lists")?;
    let list = lists::get_owned(&state.db, auth.account_id, id).await?;
    Ok(Json(list.entity()))
}

/// `PUT /api/v1/lists/{id}` — `{title?, replies_policy?, exclusive?}`.
#[utoipa::path(
    put,
    path = "/api/v1/lists/{id}",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "The updated list"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid title or replies_policy", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:lists")?;
    let params = body_params(request).await?;
    let list = lists::update(
        &state.db,
        auth.account_id,
        id,
        param(&params, "title"),
        param(&params, "replies_policy"),
        bool_param(&params, "exclusive"),
    )
    .await?
    .ok_or_else(|| Error::NotFound("list".into()))?;
    Ok(Json(list.entity()))
}

/// `DELETE /api/v1/lists/{id}`.
#[utoipa::path(
    delete,
    path = "/api/v1/lists/{id}",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "List deleted"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:lists")?;
    if !lists::delete(&state.db, auth.account_id, id).await? {
        return Err(Error::NotFound("list".into()));
    }
    Ok(Json(json!({})))
}

/// `GET /api/v1/lists/{id}/accounts` — members, paginated.
#[utoipa::path(
    get,
    path = "/api/v1/lists/{id}/accounts",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "Member accounts (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
    )
)]
pub async fn members(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:lists")?;
    let page = paginate::parse(&params);
    let rows = lists::members(&state.db, auth.account_id, id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `POST /api/v1/lists/{id}/accounts` — `{account_ids[]}`.
#[utoipa::path(
    post,
    path = "/api/v1/lists/{id}/accounts",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "Members added"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
        (status = 422, description = "Unlistable account", body = crate::error::ErrorBody),
    )
)]
pub async fn add_accounts(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:lists")?;
    let params = body_params(request).await?;
    let ids = id_list(&params, "account_ids")?;
    lists::add_accounts(&state.db, auth.account_id, id, &ids).await?;
    Ok(Json(json!({})))
}

/// `DELETE /api/v1/lists/{id}/accounts` — `{account_ids[]}`.
#[utoipa::path(
    delete,
    path = "/api/v1/lists/{id}/accounts",
    tag = "lists",
    params(("id" = String, Path, description = "List id")),
    responses(
        (status = 200, description = "Members removed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "List not found", body = crate::error::ErrorBody),
    )
)]
pub async fn remove_accounts(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:lists")?;
    let params = body_params(request).await?;
    let ids = id_list(&params, "account_ids")?;
    lists::remove_accounts(&state.db, auth.account_id, id, &ids).await?;
    Ok(Json(json!({})))
}

/// `GET /api/v1/accounts/{id}/lists` — the caller's lists containing
/// the account.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{id}/lists",
    tag = "lists",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "Lists containing the account"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn account_lists(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:lists")?;
    let rows = lists::containing(&state.db, auth.account_id, id).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|l| l.entity())
        .collect::<Vec<_>>())))
}
