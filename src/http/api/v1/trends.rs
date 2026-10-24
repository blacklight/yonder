//! `/api/v1/trends/tags` — trending hashtags with a Yonder `period`
//! extension (day/week/month) on the Mastodon-shaped route (epic 20).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde_json::{json, Value};

use crate::{
    http::{middleware::auth::MaybeAuth, AppState},
    tags::{self, entity::tag_json, trends::Period},
    Result,
};

/// `GET /api/v1/trends/tags` — ranked by uses over the window;
/// `history` is always the trailing 7 days.
#[utoipa::path(
    get,
    path = "/api/v1/trends/tags",
    tag = "trends",
    params(
        ("period" = Option<String>, Query, description = "day|week|month (default week) — yonder extension"),
        ("limit" = Option<i64>, Query, description = "Max results (default 10)"),
        ("offset" = Option<i64>, Query, description = "Skip the first n results (ranking capped at 1000)")
    ),
    responses(
        (status = 200, description = "Array of trending Tag entities"),
        (status = 422, description = "Invalid period", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn tags_trending(
    State(state): State<Arc<AppState>>,
    _auth: MaybeAuth,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let period = match params.get("period") {
        Some(p) => Period::parse(p)?.unwrap_or(Period::Week),
        None => Period::Week,
    };
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, state.config().timelines.trends_max_limit as i64);
    let offset = params
        .get("offset")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let rows =
        tags::trends::trending(&state.db, &state.cache, &state.config().timelines, period).await?;
    let page: Vec<&tags::trends::TrendRow> = rows
        .iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    let ids: Vec<i64> = page.iter().map(|r| r.tag_id).collect();
    let histories = tags::trends::history_for_many(
        &state.db,
        &ids,
        7,
        state.config().timelines.trends_include_remote,
    )
    .await?;

    let ctx = crate::posts::PostContext::new(&state);
    let base = ctx.base_url();
    let out: Vec<Value> = page
        .iter()
        .zip(histories)
        .map(|(row, history)| {
            tag_json(
                &base,
                &tags::TagInfo {
                    id: row.tag_id,
                    name: row.name.clone(),
                    following: false,
                    history,
                },
            )
        })
        .collect();
    Ok(Json(json!(out)))
}
