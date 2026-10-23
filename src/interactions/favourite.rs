//! Favourites — `favourites` rows, `favourites_count`, outbound
//! `Like`/`Undo(Like)` for remote targets.

use sqlx::Row;

use crate::{
    activitypub::{delivery, ApContext},
    http::middleware::auth::AuthContext,
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

use super::{
    account_parts, emit_notification_event, load_visible, new_activity_id, stored_activity,
};

/// `POST /api/v1/statuses/{id}/favourite`. Idempotent — a repeat
/// favourite returns the current entity.
pub async fn favourite(ctx: &PostContext, auth: &AuthContext, status_id: i64) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let mut tx = ctx.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO favourites (account_id, status_id) VALUES ($1,$2) \
         ON CONFLICT (account_id, status_id) DO NOTHING",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted > 0 {
        sqlx::query("UPDATE statuses SET favourites_count = favourites_count + 1 WHERE id = $1")
            .bind(status.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Publish only on a new row — replayed favourites don't mint a
    // second Like activity (matches the boost service's replay).
    if let (Some(ap), false, true) = (&ctx.ap, status.local, inserted > 0) {
        if let Err(e) = publish_like(ctx, ap, auth.account_id, &status).await {
            tracing::error!(status_id = status.id, error = %e, "AP Like publish failed");
        }
    }
    if inserted > 0 {
        emit_notification_event(
            &ctx.db,
            &ctx.dispatcher,
            "favourite",
            auth.account_id,
            status.id,
            status.account_id,
        )
        .await;
    }
    posts::load(&ctx.db, status.id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after favourite".into()))
}

/// `POST /api/v1/statuses/{id}/unfavourite`. Idempotent — returns the
/// entity even when nothing was favourited.
pub async fn unfavourite(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let mut tx = ctx.db.begin().await?;
    let removed = sqlx::query(
        "DELETE FROM favourites WHERE account_id = $1 AND status_id = $2 \
         RETURNING activity_uri",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .fetch_optional(&mut *tx)
    .await?;
    if removed.is_some() {
        sqlx::query(
            "UPDATE statuses SET favourites_count = GREATEST(favourites_count - 1, 0) WHERE id = $1",
        )
        .bind(status.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let activity_uri: Option<String> = removed
        .as_ref()
        .map(|r| r.get::<Option<String>, _>("activity_uri"))
        .unwrap_or(None);
    if let (Some(ap), Some(activity_uri)) = (&ctx.ap, activity_uri) {
        if let Err(e) = publish_undo_like(ctx, ap, auth.account_id, &status, &activity_uri).await {
            tracing::error!(status_id = status.id, error = %e, "AP Undo(Like) publish failed");
        }
    }
    posts::load(&ctx.db, status.id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after unfavourite".into()))
}

/// The account's liked statuses, newest first (cursor =
/// `favourites.id`). Tombstoned targets are excluded *in* the page
/// query so cursors stay exact.
pub async fn list(
    ctx: &PostContext,
    account_id: i64,
    page: &crate::http::paginate::Page,
) -> Result<Vec<(StatusRow, i64)>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT f.status_id, f.id FROM favourites f \
         JOIN statuses s ON s.id = f.status_id AND NOT s.tombstone \
         WHERE f.account_id = $1 \
           AND ($2::bigint IS NULL OR f.id < $2) \
           AND ($3::bigint IS NULL OR f.id > $3) \
         ORDER BY f.id DESC LIMIT $4",
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

/// Account ids of the likers, newest first.
pub async fn favourited_by(
    ctx: &PostContext,
    status: &StatusRow,
    page: &crate::http::paginate::Page,
) -> Result<Vec<(i64, i64)>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT account_id, id FROM favourites \
         WHERE status_id = $1 \
           AND ($2::bigint IS NULL OR id < $2) \
           AND ($3::bigint IS NULL OR id > $3) \
         ORDER BY id DESC LIMIT $4",
    )
    .bind(status.id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows)
}

/// `Like` addressed to the target's author (`to`), with
/// `cc=[Public, followers]` — the Mastodon/Songhive convention.
/// `favourites.activity_uri` records the sent id for `Undo(Like)`.
async fn publish_like(
    ctx: &PostContext,
    ap: &ApContext,
    account_id: i64,
    status: &StatusRow,
) -> Result<()> {
    let Some(uri) = status.uri.clone() else {
        return Ok(());
    };
    let (username, actor_id) = account_parts(ctx, account_id).await?;
    let (_, author_actor) = account_parts(ctx, status.account_id).await?;
    let activity = pubby_sdk::build_like_activity(
        &actor_id,
        &uri,
        Some(vec![author_actor]),
        Some(vec![
            pubby_sdk::AS_PUBLIC.to_string(),
            format!("{actor_id}/followers"),
        ]),
        Some(&new_activity_id(&actor_id)),
        None,
        None,
    );
    let stored = delivery::publish(ap, &username, &activity).await?;
    if let Some(id) = stored.get("id").and_then(|v| v.as_str()) {
        sqlx::query(
            "UPDATE favourites SET activity_uri = $3 WHERE account_id = $1 AND status_id = $2",
        )
        .bind(account_id)
        .bind(status.id)
        .bind(id)
        .execute(&ctx.db)
        .await?;
    }
    Ok(())
}

/// `Undo(Like)` — rebuild the inner `Like` from the stored
/// `activities` row, or deterministically when it is missing.
async fn publish_undo_like(
    ctx: &PostContext,
    ap: &ApContext,
    account_id: i64,
    status: &StatusRow,
    activity_uri: &str,
) -> Result<()> {
    let Some(uri) = status.uri.clone() else {
        return Ok(());
    };
    let (username, actor_id) = account_parts(ctx, account_id).await?;
    let (_, author_actor) = account_parts(ctx, status.account_id).await?;
    let inner = stored_activity(&ctx.db, activity_uri)
        .await?
        .unwrap_or_else(|| {
            pubby_sdk::build_like_activity(
                &actor_id,
                &uri,
                Some(vec![author_actor.clone()]),
                Some(vec![pubby_sdk::AS_PUBLIC.to_string()]),
                Some(activity_uri),
                None,
                None,
            )
        });
    let undo = pubby_sdk::build_undo_activity(
        &inner,
        &actor_id,
        Some(&new_activity_id(&actor_id)),
        None,
        Some(vec![author_actor]),
        Some(vec![pubby_sdk::AS_PUBLIC.to_string()]),
        None,
    );
    delivery::publish(ap, &username, &undo).await?;
    Ok(())
}
