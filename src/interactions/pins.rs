//! Pinned posts — `status_pins` + the `featured` collection.

use crate::{
    activitypub::featured,
    http::middleware::auth::AuthContext,
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

/// `POST /api/v1/statuses/{id}/pin` — own statuses only, not boosts,
/// not `direct`; `content.max_pinned_posts` caps the count. Idempotent.
pub async fn pin(ctx: &PostContext, auth: &AuthContext, status_id: i64) -> Result<StatusRow> {
    let status = posts::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if status.account_id != auth.account_id {
        return Err(Error::Forbidden);
    }
    if status.reblog_of_id.is_some() {
        return Err(Error::Validation("a boost cannot be pinned".to_string()));
    }
    if status.visibility == "direct" {
        return Err(Error::Validation(
            "a direct status cannot be pinned".to_string(),
        ));
    }
    let mut tx = ctx.db.begin().await?;
    // Serialize pin ops per account — row locks can't cover the
    // not-yet-inserted first pin.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(auth.account_id)
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM status_pins WHERE account_id = $1")
        .bind(auth.account_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= ctx.config.content.max_pinned_posts as i64 {
        return Err(Error::Validation(format!(
            "at most {} posts can be pinned",
            ctx.config.content.max_pinned_posts
        )));
    }
    sqlx::query(
        "INSERT INTO status_pins (account_id, status_id, idx) \
         VALUES ($1,$2, COALESCE((SELECT max(idx) + 1 FROM status_pins WHERE account_id = $1), 0)) \
         ON CONFLICT (account_id, status_id) DO NOTHING",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    invalidate(ctx, auth.account_id).await;
    Ok(status)
}

/// `POST /api/v1/statuses/{id}/unpin` — idempotent.
pub async fn unpin(ctx: &PostContext, auth: &AuthContext, status_id: i64) -> Result<StatusRow> {
    let status = posts::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if status.account_id != auth.account_id {
        return Err(Error::Forbidden);
    }
    sqlx::query("DELETE FROM status_pins WHERE account_id = $1 AND status_id = $2")
        .bind(auth.account_id)
        .bind(status.id)
        .execute(&ctx.db)
        .await?;
    invalidate(ctx, auth.account_id).await;
    Ok(status)
}

/// The ids an account has pinned, ordered (`idx`, insert order).
pub async fn pinned_ids(ctx: &PostContext, account_id: i64) -> Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT status_id FROM status_pins WHERE account_id = $1 ORDER BY idx, status_id",
    )
    .bind(account_id)
    .fetch_all(&ctx.db)
    .await
    .map_err(Into::into)
}

/// Drop the featured-collection doc-cache entry for `account_id`'s
/// actor — best-effort (no federation → nothing to invalidate).
async fn invalidate(ctx: &PostContext, account_id: i64) {
    let Some(ap) = &ctx.ap else { return };
    match super::account_parts(ctx, account_id).await {
        Ok((username, _)) => {
            ap.core
                .doc_cache
                .invalidate(&[featured::featured_key(&username)]);
        }
        Err(e) => tracing::warn!(error = %e, "featured cache invalidation failed"),
    }
}
