//! Bookmarks — private saves. No AP activity, no notification event
//! (Akkoma semantics).

use crate::{
    http::middleware::auth::AuthContext,
    posts::{self, PostContext, StatusRow},
    Result,
};

use super::load_visible;

/// `POST /api/v1/statuses/{id}/bookmark` — idempotent.
pub async fn bookmark(ctx: &PostContext, auth: &AuthContext, status_id: i64) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    sqlx::query(
        "INSERT INTO bookmarks (account_id, status_id) VALUES ($1,$2) \
         ON CONFLICT (account_id, status_id) DO NOTHING",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .execute(&ctx.db)
    .await?;
    Ok(status)
}

/// `POST /api/v1/statuses/{id}/unbookmark` — idempotent.
pub async fn unbookmark(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    sqlx::query("DELETE FROM bookmarks WHERE account_id = $1 AND status_id = $2")
        .bind(auth.account_id)
        .bind(status.id)
        .execute(&ctx.db)
        .await?;
    Ok(status)
}

/// The account's bookmarks, newest first (cursor = `bookmarks.id`).
/// Tombstoned targets are excluded *in* the page query so cursors
/// stay exact.
pub async fn list(
    ctx: &PostContext,
    account_id: i64,
    page: &crate::http::paginate::Page,
) -> Result<Vec<(StatusRow, i64)>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT b.status_id, b.id FROM bookmarks b \
         JOIN statuses s ON s.id = b.status_id AND NOT s.tombstone \
         WHERE b.account_id = $1 \
           AND ($2::bigint IS NULL OR b.id < $2) \
           AND ($3::bigint IS NULL OR b.id > $3) \
         ORDER BY b.id DESC LIMIT $4",
    )
    .bind(account_id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (status_id, cursor) in rows {
        if let Some(status) = posts::load(&ctx.db, status_id).await? {
            out.push((status, cursor));
        }
    }
    Ok(out)
}
