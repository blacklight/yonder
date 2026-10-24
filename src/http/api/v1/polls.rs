//! `/api/v1/polls` — results and votes (epic 12).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    response::Json,
};
use serde_json::Value;

use crate::{
    auth::scopes::require_scope,
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        AppState,
    },
    posts::{polls, PostContext},
    Result,
};

/// `GET /api/v1/polls/{id}` — results; `voted`/`own_votes` only when
/// authenticated.
#[utoipa::path(
    get,
    path = "/api/v1/polls/{id}",
    tag = "polls",
    params(("id" = String, Path, description = "Poll id")),
    responses(
        (status = 200, description = "The poll"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let ctx = PostContext::new(&state);
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    if !polls::status_visible(&ctx, id, viewer).await? {
        return Err(crate::Error::NotFound("poll".into()));
    }
    Ok(Json(polls::entity(&ctx, id, viewer).await?))
}

/// `POST /api/v1/polls/{id}/votes` — `{choices: [idx…]}`. Atomic:
/// any invalid/duplicate choice rolls the request back.
#[utoipa::path(
    post,
    path = "/api/v1/polls/{id}/votes",
    tag = "polls",
    params(("id" = String, Path, description = "Poll id")),
    request_body(content_type = "application/json", content = VoteSchema),
    responses(
        (status = 200, description = "The poll after voting"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid or duplicate vote", body = crate::error::ErrorBody),
    )
)]
pub async fn vote(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let choices: Vec<i64> = body
        .get("choices")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    c.as_i64()
                        .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
                })
                .collect()
        })
        .unwrap_or_default();
    polls::vote(&ctx, id, auth.account_id, &choices).await?;
    Ok(Json(polls::entity(&ctx, id, Some(auth.account_id)).await?))
}

/// `choices` body shape.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct VoteSchema {
    /// Option indexes.
    pub choices: Vec<i64>,
}
