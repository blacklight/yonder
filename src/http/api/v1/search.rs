//! `GET /api/v1/search` — Mastodon v1 search (epic 17). Same
//! pipeline as v2 (`crate::search`); only the hashtag shape differs
//! (v1 returns plain strings).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde_json::Value;

use crate::{
    http::{middleware::auth::MaybeAuth, AppState},
    search, Result,
};

/// `GET /api/v1/search` — accounts/statuses/hashtags with the v1
/// entity shapes.
#[utoipa::path(
    get,
    path = "/api/v1/search",
    tag = "search",
    params(
        ("q" = String, Query, description = "Search query: text, #tag, @user@domain or URL"),
        ("type" = Option<String>, Query, description = "accounts|statuses|hashtags"),
        ("resolve" = Option<bool>, Query, description = "Attempt WebFinger/AP remote lookup (auth required)"),
        ("following" = Option<bool>, Query, description = "Only accounts the caller follows"),
        ("account_id" = Option<i64>, Query, description = "Only statuses by this author"),
        ("offset" = Option<i64>, Query, description = "Skip this many results (default 0)"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Search results grouped by entity type"),
        (status = 400, description = "Missing/invalid query", body = crate::error::ErrorBody),
        (status = 401, description = "Anonymous search disabled", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn search_v1(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let caller = search::authorize(&state, auth.0)?;
    let parsed = search::params::parse(&params)?;
    let results = search::search(&state, caller.as_ref(), &parsed).await?;
    let body = search::render(
        &state,
        &results,
        caller.as_ref().map(|a| a.account_id),
        true,
    )
    .await?;
    Ok(Json(body))
}
