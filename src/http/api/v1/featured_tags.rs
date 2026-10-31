//! `/api/v1/featured_tags` — Mastodon featured hashtags: tags a user
//! pins on their profile. `GET /api/v1/accounts/{id}/featured_tags`
//! (in `accounts.rs`) reads the same list for another account.

use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::header,
    Json,
};
use serde_json::{json, Value};

use crate::{
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// The FeaturedTag entity — `statuses_count`/`last_status_at` count
/// the owner's public statuses carrying the tag.
async fn entity(db: &sqlx::PgPool, ft_id: i64, account_id: i64) -> Result<Value> {
    let row = sqlx::query(
        "SELECT ft.id, t.name, \
                (SELECT count(*) FROM status_tags st JOIN statuses s ON s.id = st.status_id \
                 WHERE st.tag_id = ft.tag_id AND s.account_id = ft.account_id \
                   AND s.visibility = 'public' AND NOT s.tombstone) AS statuses_count, \
                (SELECT max(s.created_at) FROM status_tags st JOIN statuses s ON s.id = st.status_id \
                 WHERE st.tag_id = ft.tag_id AND s.account_id = ft.account_id \
                   AND s.visibility = 'public' AND NOT s.tombstone) AS last_status_at \
         FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id \
         WHERE ft.id = $1 AND ft.account_id = $2",
    )
    .bind(ft_id)
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("featured_tag".into()))?;
    use sqlx::Row;
    let last: Option<chrono::DateTime<chrono::Utc>> = row.get("last_status_at");
    Ok(json!({
        "id": row.get::<i64, _>("id").to_string(),
        "name": row.get::<String, _>("name"),
        "url": Value::Null,
        "statuses_count": row.get::<i64, _>("statuses_count"),
        "last_status_at": last.map(|t| t.to_rfc3339()),
    }))
}

/// The shared read — `accounts/{id}/featured_tags` calls it too.
pub(crate) async fn list_for(state: &Arc<AppState>, account_id: i64) -> Result<Json<Value>> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM featured_tags WHERE account_id = $1 ORDER BY id")
            .bind(account_id)
            .fetch_all(&state.db)
            .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(entity(&state.db, id, account_id).await?);
    }
    Ok(Json(json!(out)))
}

/// `GET /api/v1/featured_tags` — the caller's featured list.
#[utoipa::path(
    get,
    path = "/api/v1/featured_tags",
    tag = "featured_tags",
    responses(
        (status = 200, description = "Array of FeaturedTag entities"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "read:accounts")?;
    list_for(&state, auth.account_id).await
}

/// `POST /api/v1/featured_tags` — feature `name` (form or JSON).
/// Idempotent: featuring an already-featured tag is a no-op 200.
#[utoipa::path(
    post,
    path = "/api/v1/featured_tags",
    tag = "featured_tags",
    responses(
        (status = 200, description = "The FeaturedTag entity"),
        (status = 422, description = "Missing/unknown tag", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "write:accounts")?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    let name = if content_type.starts_with("application/x-www-form-urlencoded") {
        url::form_urlencoded::parse(&bytes)
            .find(|(k, _)| k == "name")
            .map(|(_, v)| v.into_owned())
    } else {
        serde_json::from_slice::<Value>(&bytes)
            .unwrap_or_else(|_| json!({}))
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let name = name
        .map(|n| n.trim().trim_start_matches('#').to_lowercase())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| Error::Validation("name is required".into()))?;
    // The tag must exist already (it's drawn from the account's own
    // usage — Mastodon suggests, never invents); suggest-free inserts
    // would mint orphaned rows.
    let tag_id: i64 = sqlx::query_scalar("SELECT id FROM tags WHERE lower(name) = lower($1)")
        .bind(&name)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| Error::Validation(format!("unknown hashtag '{name}'")))?;
    let ft_id: i64 = sqlx::query_scalar(
        "INSERT INTO featured_tags (account_id, tag_id) VALUES ($1,$2) \
         ON CONFLICT (account_id, tag_id) DO UPDATE SET created_at = featured_tags.created_at \
         RETURNING id",
    )
    .bind(auth.account_id)
    .bind(tag_id)
    .fetch_one(&state.db)
    .await?;
    entity(&state.db, ft_id, auth.account_id).await.map(Json)
}

/// `DELETE /api/v1/featured_tags/{id}` — unfeature; owner-scoped 404.
#[utoipa::path(
    delete,
    path = "/api/v1/featured_tags/{id}",
    tag = "featured_tags",
    params(("id" = String, Path, description = "FeaturedTag id")),
    responses(
        (status = 200, description = "Unfeatured"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn unfeature(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "write:accounts")?;
    let affected = sqlx::query("DELETE FROM featured_tags WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(auth.account_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(Error::NotFound("featured_tag".into()));
    }
    Ok(Json(json!({})))
}

/// `GET /api/v1/featured_tags/suggestions` — the caller's most-used
/// tags not already featured (Mastodon's suggestion heuristic).
#[utoipa::path(
    get,
    path = "/api/v1/featured_tags/suggestions",
    tag = "featured_tags",
    responses(
        (status = 200, description = "Array of Tag entities"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn suggestions(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "read:accounts")?;
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT t.id, t.name FROM status_tags st \
         JOIN statuses s ON s.id = st.status_id \
         JOIN tags t ON t.id = st.tag_id \
         WHERE s.account_id = $1 AND s.visibility = 'public' AND NOT s.tombstone \
           AND NOT EXISTS (SELECT 1 FROM featured_tags ft \
                           WHERE ft.account_id = $1 AND ft.tag_id = t.id) \
         GROUP BY t.id, t.name ORDER BY count(*) DESC, t.name LIMIT 10",
    )
    .bind(auth.account_id)
    .fetch_all(&state.db)
    .await?;
    let base = crate::posts::PostContext::new(&state).base_url();
    Ok(Json(json!(rows
        .iter()
        .map(|(id, name)| crate::tags::entity::tag_json(
            &base,
            &crate::tags::TagInfo {
                id: *id,
                name: name.clone(),
                following: false,
                history: Vec::new(),
            },
        ))
        .collect::<Vec<_>>())))
}
