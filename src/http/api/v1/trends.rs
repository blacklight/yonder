//! `/api/v1/trends/tags` — trending hashtags with a Yonder `period`
//! extension (day/week/month) on the Mastodon-shaped route (epic 20).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;

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

/// `GET /api/v1/trends` — the deprecated pre-3.0 alias of
/// `trends/tags` (Moshidon and older apps still call it).
#[utoipa::path(
    get,
    path = "/api/v1/trends",
    tag = "trends",
    params(("limit" = Option<i64>, Query, description = "Max results (default 10)")),
    responses((status = 200, description = "Array of trending Tag entities"))
)]
pub async fn trends_legacy(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    tags_trending(State(state), auth, Query(params)).await
}

/// `GET /api/v1/trends/statuses` — public statuses ranked by
/// favourites + boosts over the window (`period`, same day/week/
/// month extension as `trends/tags`). Public reads only; the viewer
/// gets fully rendered entities.
#[utoipa::path(
    get,
    path = "/api/v1/trends/statuses",
    tag = "trends",
    params(
        ("period" = Option<String>, Query, description = "day|week|month (default week) — yonder extension"),
        ("limit" = Option<i64>, Query, description = "Max results (default 10)"),
        ("offset" = Option<i64>, Query, description = "Skip the first n results")
    ),
    responses(
        (status = 200, description = "Array of trending Status entities"),
        (status = 422, description = "Invalid period", body = crate::error::ErrorBody),
    )
)]
pub async fn statuses_trending(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
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
    let viewer = auth.0.as_ref().map(|a| a.account_id);

    // Interaction rows in the window drive the ranking: a favourite
    // and a boost count one point each toward the target.
    let window = format!("{} days", period.days());
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT target, count(*) AS score FROM ( \
             SELECT status_id AS target FROM favourites \
             WHERE created_at > now() - $2::interval \
             UNION ALL \
             SELECT reblog_of_id AS target FROM statuses \
             WHERE reblog_of_id IS NOT NULL AND created_at > now() - $2::interval \
         ) hits \
         JOIN statuses s ON s.id = target \
         WHERE s.visibility = 'public' AND NOT s.tombstone \
         GROUP BY target ORDER BY score DESC, target DESC \
         LIMIT $1 OFFSET $3",
    )
    .bind(limit)
    .bind(&window)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let by_id = crate::posts::load_many(&state.db, &ids).await?;
    // `load_many` is hash-keyed — restore the ranking order.
    let rows: Vec<_> = ids.iter().filter_map(|i| by_id.get(i).cloned()).collect();
    let ctx = crate::posts::PostContext::new(&state);
    Ok(Json(json!(
        crate::posts::entity::status_entities(&ctx, &rows, viewer).await?
    )))
}

/// `GET /api/v1/trends/links` — preview cards ranked by the number of
/// public statuses sharing them in the window. `history` is the
/// trailing 7 days of uses/accounts (Mastodon's `Trends::Link`
/// shape).
#[utoipa::path(
    get,
    path = "/api/v1/trends/links",
    tag = "trends",
    params(
        ("period" = Option<String>, Query, description = "day|week|month (default week) — yonder extension"),
        ("limit" = Option<i64>, Query, description = "Max results (default 10)"),
        ("offset" = Option<i64>, Query, description = "Skip the first n results")
    ),
    responses(
        (status = 200, description = "Array of trending PreviewCard entities"),
        (status = 422, description = "Invalid period", body = crate::error::ErrorBody),
    )
)]
pub async fn links_trending(
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

    // Rank distinct shared URLs; per URL the newest card row supplies
    // the entity fields.
    let window = format!("{} days", period.days());
    let urls: Vec<String> = sqlx::query_scalar(
        "SELECT p.url, count(DISTINCT s.id) AS uses \
         FROM preview_cards p \
         JOIN statuses s ON s.id = p.status_id \
         WHERE s.visibility = 'public' AND NOT s.tombstone \
           AND s.created_at > now() - $2::interval \
         GROUP BY p.url ORDER BY uses DESC, p.url \
         LIMIT $1 OFFSET $3",
    )
    .bind(limit)
    .bind(&window)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    if urls.is_empty() {
        return Ok(Json(json!([])));
    }
    let cards = sqlx::query(
        "SELECT DISTINCT ON (url) url, title, description, image_path, \
                author_name, author_url, provider_name, provider_url, \
                embed_url, embed_width, embed_height \
         FROM preview_cards WHERE url = ANY($1) \
         ORDER BY url, id DESC",
    )
    .bind(&urls)
    .fetch_all(&state.db)
    .await?;
    let history = sqlx::query_as::<_, (String, String, i64, i64)>(
        "SELECT p.url, to_char(date_trunc('day', s.created_at AT TIME ZONE 'UTC'), \
                'YYYY-MM-DD') AS day, \
                count(*)::bigint AS uses, count(DISTINCT s.account_id)::bigint AS accounts \
         FROM preview_cards p \
         JOIN statuses s ON s.id = p.status_id \
         WHERE p.url = ANY($1) AND s.visibility = 'public' AND NOT s.tombstone \
           AND s.created_at > now() - interval '7 days' \
         GROUP BY p.url, day ORDER BY p.url, day DESC",
    )
    .bind(&urls)
    .fetch_all(&state.db)
    .await?;

    let ctx = crate::posts::PostContext::new(&state);
    let base = ctx.base_url();
    let mut history_by_url: HashMap<String, Vec<Value>> = HashMap::new();
    for (url, day, uses, accounts) in history {
        history_by_url.entry(url).or_default().push(json!({
            "day": day,
            "uses": uses.to_string(),
            "accounts": accounts.to_string(),
        }));
    }
    let mut by_url: HashMap<String, Value> = HashMap::new();
    for r in &cards {
        let url: String = r.get("url");
        let image: Option<String> = r.get("image_path");
        let embed_url: String = r.get("embed_url");
        by_url.insert(
            url.clone(),
            json!({
                "url": url,
                "title": r.get::<String, _>("title"),
                "description": r.get::<String, _>("description"),
                "type": if embed_url.is_empty() { "link" } else { "video" },
                "author_name": r.get::<String, _>("author_name"),
                "author_url": r.get::<String, _>("author_url"),
                "provider_name": r.get::<String, _>("provider_name"),
                "provider_url": r.get::<String, _>("provider_url"),
                "html": "",
                "width": r.get::<i32, _>("embed_width"),
                "height": r.get::<i32, _>("embed_height"),
                "image": image.map(|k| crate::media::public_url(&state.config(), &base, &k)),
                "embed_url": embed_url,
                "blurhash": Value::Null,
                "history": history_by_url.remove(&url).unwrap_or_default(),
            }),
        );
    }
    let out: Vec<Value> = urls.iter().filter_map(|u| by_url.remove(u)).collect();
    Ok(Json(json!(out)))
}
