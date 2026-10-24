//! `GET /api/v1/custom_emojis` — the public emoji catalogue (epic 20).
//! Anonymous, unscoped — every client needs it to render `:shortcode:`.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::Value;

use crate::{emoji::entity::public_list, http::AppState, posts::PostContext, Result};

/// `GET /api/v1/custom_emojis` — enabled, picker-visible local emoji.
#[utoipa::path(
    get,
    path = "/api/v1/custom_emojis",
    tag = "custom emojis",
    responses(
        (status = 200, description = "Array of CustomEmoji entities"),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let ctx = PostContext::new(&state);
    let rows = public_list(&state.db, &state.config, &ctx.base_url()).await?;
    Ok(Json(Value::Array(rows)))
}
