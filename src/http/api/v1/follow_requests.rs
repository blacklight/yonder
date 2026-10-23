//! `/api/v1/follow_requests` — the locked-account approval queue
//! (epic 14). `:id` is the *requester's* account id, matching Mastodon.

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
    http::{middleware::auth::AuthContext, paginate, AppState},
    social::requests,
    Result,
};

use super::relationships::account_entities;

/// `GET /api/v1/follow_requests` — accounts with a pending request on
/// the caller, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/follow_requests",
    tag = "follow_requests",
    responses(
        (status = 200, description = "Pending requesters (Link-header paginated)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config, &auth, "read:follows")?;
    let page = paginate::parse(&params);
    let rows = requests::list(&state.db, auth.account_id, &page).await?;
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `POST /api/v1/follow_requests/{id}/authorize` — approve a pending
/// request. Returns the caller's Relationship with the requester.
#[utoipa::path(
    post,
    path = "/api/v1/follow_requests/{id}/authorize",
    tag = "follow_requests",
    params(("id" = String, Path, description = "Requester account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No pending request", body = crate::error::ErrorBody),
    )
)]
pub async fn authorize(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    requests::authorize(&state, auth.account_id, id).await?;
    Ok(Json(
        crate::social::entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/follow_requests/{id}/reject` (and the legacy
/// `DELETE /api/v1/follow_requests/{id}`) — decline a pending request.
#[utoipa::path(
    post,
    path = "/api/v1/follow_requests/{id}/reject",
    tag = "follow_requests",
    params(("id" = String, Path, description = "Requester account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No pending request", body = crate::error::ErrorBody),
    )
)]
pub async fn reject(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    requests::reject(&state, auth.account_id, id).await?;
    Ok(Json(
        crate::social::entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}
