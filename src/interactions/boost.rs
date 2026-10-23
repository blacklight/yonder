//! Boosts — reblog `statuses` rows (`reblog_of_id`), outbound
//! `Announce`/`Undo(Announce)`.

use crate::{
    activitypub::{delivery, ApContext},
    http::middleware::auth::AuthContext,
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

use super::{
    account_parts, emit_notification_event, load_visible, new_activity_id, stored_activity,
};

/// `POST /api/v1/statuses/{id}/reblog` — idempotent (replays the
/// existing boost), gated to `public`/`unlisted` targets (Mastodon
/// semantics: private/direct posts cannot be boosted).
/// `visibility` is the optional boost audience (`public|unlisted`,
/// default `public`).
pub async fn reblog(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
    visibility: Option<&str>,
) -> Result<StatusRow> {
    let target = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    // Reblog-of-reblog normalizes to the original post.
    let target = match target.reblog_of_id {
        Some(orig) => posts::load(&ctx.db, orig)
            .await?
            .ok_or_else(|| Error::NotFound("status".into()))?,
        None => target,
    };
    if !matches!(target.visibility.as_str(), "public" | "unlisted") {
        return Err(Error::Validation(
            "this status cannot be boosted".to_string(),
        ));
    }
    let boost_visibility = match visibility {
        None | Some("public") => "public",
        Some("unlisted") => "unlisted",
        Some(other) => {
            return Err(Error::Validation(format!(
                "visibility must be public or unlisted (got {other:?})"
            )))
        }
    };

    // Replay: an existing live boost row returns 200 with its entity.
    if let Some(id) = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM statuses \
         WHERE account_id = $1 AND reblog_of_id = $2 AND NOT tombstone",
    )
    .bind(auth.account_id)
    .bind(target.id)
    .fetch_optional(&ctx.db)
    .await?
    {
        return posts::load(&ctx.db, id)
            .await?
            .ok_or_else(|| Error::Internal("boost missing after replay".into()));
    }

    let (username, actor_id) = account_parts(ctx, auth.account_id).await?;
    let announce_uri = new_activity_id(&actor_id);
    let mut tx = ctx.db.begin().await?;
    let boost_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, content_type, visibility, \
             local, reblog_of_id, uri, url) \
         VALUES ($1,'','','text/plain',$2,true,$3,$4,'') \
         ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL \
         DO NOTHING RETURNING id",
    )
    .bind(auth.account_id)
    .bind(boost_visibility)
    .bind(target.id)
    .bind(&announce_uri)
    .fetch_optional(&mut *tx)
    .await?;
    // Only the insert winner fans out — a loser's announce_uri
    // matches no stored row, and the winner already emits the
    // notification event.
    let won_insert = boost_id.is_some();
    if let Some(boost_id) = boost_id {
        sqlx::query("UPDATE statuses SET reblogs_count = reblogs_count + 1 WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
        let (_, url) = ctx.status_urls(&username, boost_id);
        sqlx::query("UPDATE statuses SET url = $2 WHERE id = $1")
            .bind(boost_id)
            .bind(&url)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // The unique index guarantees a row exists — either the one we
    // just inserted or a concurrent winner's.
    let boost_id = match boost_id {
        Some(id) => id,
        None => {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM statuses \
             WHERE account_id = $1 AND reblog_of_id = $2 AND NOT tombstone",
            )
            .bind(auth.account_id)
            .bind(target.id)
            .fetch_one(&ctx.db)
            .await?
        }
    };

    if won_insert {
        if let Some(ap) = &ctx.ap {
            if let Err(e) = publish_announce(
                ctx,
                ap,
                &actor_id,
                &username,
                &announce_uri,
                &target,
                boost_visibility,
            )
            .await
            {
                tracing::error!(status_id = target.id, error = %e, "AP Announce publish failed");
            }
        }
        emit_notification_event(
            &ctx.db,
            &ctx.dispatcher,
            "reblog",
            auth.account_id,
            target.id,
            target.account_id,
        )
        .await;
    }
    posts::load(&ctx.db, boost_id)
        .await?
        .ok_or_else(|| Error::Internal("boost missing after insert".into()))
}

/// `POST /api/v1/statuses/{id}/unreblog` — `{id}` is the original
/// status id (Mastodon semantics; a boost row id resolves through its
/// `reblog_of_id`). Idempotent.
pub async fn unreblog(ctx: &PostContext, auth: &AuthContext, status_id: i64) -> Result<StatusRow> {
    let given = posts::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    let target_id = given.reblog_of_id.unwrap_or(given.id);
    let boost_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM statuses \
         WHERE account_id = $1 AND reblog_of_id = $2 AND NOT tombstone",
    )
    .bind(auth.account_id)
    .bind(target_id)
    .fetch_optional(&ctx.db)
    .await?;
    if let Some(boost_id) = boost_id {
        let boost = posts::load(&ctx.db, boost_id)
            .await?
            .ok_or_else(|| Error::Internal("boost missing".into()))?;
        unreblog_row(ctx, &boost).await?;
    }
    posts::load(&ctx.db, target_id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after unreblog".into()))
}

/// Tombstone a boost row and publish `Undo(Announce)` — shared by
/// `unreblog` and owner `DELETE` of a boost status.
pub async fn unreblog_row(ctx: &PostContext, boost: &StatusRow) -> Result<StatusRow> {
    let Some(target_id) = boost.reblog_of_id else {
        return Err(Error::Validation("not a boost".into()));
    };
    let mut tx = ctx.db.begin().await?;
    // The counter moves only when the tombstone transition wins —
    // a stale `unreblog_row` call (racing Undo, repeat DELETE) must
    // not decrement a concurrent boost's count.
    let tombstoned = sqlx::query(
        "UPDATE statuses SET tombstone = true, updated_at = now() \
         WHERE id = $1 AND NOT tombstone",
    )
    .bind(boost.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if tombstoned > 0 {
        sqlx::query(
            "UPDATE statuses SET reblogs_count = GREATEST(reblogs_count - 1, 0) WHERE id = $1",
        )
        .bind(target_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    if let Some(ap) = &ctx.ap {
        if let Err(e) = publish_undo_announce(ctx, ap, boost).await {
            tracing::error!(status_id = boost.id, error = %e, "AP Undo(Announce) publish failed");
        }
    }
    posts::load_any(&ctx.db, boost.id)
        .await?
        .ok_or_else(|| Error::Internal("boost missing after unreblog".into()))
}

/// Account ids of the boosters of `target`, newest first (cursor =
/// the boost row id).
pub async fn reblogged_by(
    ctx: &PostContext,
    target: &StatusRow,
    page: &crate::http::paginate::Page,
) -> Result<Vec<(i64, i64)>> {
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT account_id, id FROM statuses \
         WHERE reblog_of_id = $1 AND NOT tombstone \
           AND ($2::bigint IS NULL OR id < $2) \
           AND ($3::bigint IS NULL OR id > $3) \
         ORDER BY id DESC LIMIT $4",
    )
    .bind(target.id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows)
}

/// `Announce` for a local boost: `public` → `to=[Public]`,
/// `cc=[followers, author]`; `unlisted` → `to=[followers]`,
/// `cc=[author]`. The original author is always addressed.
async fn publish_announce(
    ctx: &PostContext,
    ap: &ApContext,
    actor_id: &str,
    username: &str,
    announce_uri: &str,
    target: &StatusRow,
    visibility: &str,
) -> Result<()> {
    let Some(uri) = target.uri.clone() else {
        return Ok(());
    };
    let (_, author_actor) = account_parts(ctx, target.account_id).await?;
    let followers = format!("{actor_id}/followers");
    let (to, cc) = if visibility == "unlisted" {
        (vec![followers.clone()], vec![author_actor])
    } else {
        (
            vec![pubby_sdk::AS_PUBLIC.to_string()],
            vec![followers, author_actor],
        )
    };
    let activity = pubby_sdk::build_announce_activity(
        actor_id,
        &uri,
        Some(to),
        Some(cc),
        Some(announce_uri),
        None,
        None,
    );
    delivery::publish(ap, username, &activity).await?;
    Ok(())
}

/// `Undo(Announce)` — the inner Announce is rebuilt from the stored
/// `activities` row (the boost's `uri`), falling back to a
/// deterministic rebuild.
async fn publish_undo_announce(ctx: &PostContext, ap: &ApContext, boost: &StatusRow) -> Result<()> {
    let Some(announce_uri) = boost.uri.clone() else {
        return Ok(());
    };
    let Some(target_id) = boost.reblog_of_id else {
        return Ok(());
    };
    let target_uri: Option<String> = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
        .bind(target_id)
        .fetch_optional(&ctx.db)
        .await?
        .flatten();
    let (username, actor_id) = account_parts(ctx, boost.account_id).await?;
    let followers = format!("{actor_id}/followers");
    // Fallback rebuild keeps the boost row's own addressing.
    let inner_to = if boost.visibility == "unlisted" {
        vec![followers.clone()]
    } else {
        vec![pubby_sdk::AS_PUBLIC.to_string()]
    };
    let inner = stored_activity(&ctx.db, &announce_uri)
        .await?
        .unwrap_or_else(|| {
            pubby_sdk::build_announce_activity(
                &actor_id,
                &target_uri.clone().unwrap_or_default(),
                Some(inner_to),
                Some(vec![followers.clone()]),
                Some(&announce_uri),
                None,
                None,
            )
        });
    let undo = pubby_sdk::build_undo_activity(
        &inner,
        &actor_id,
        Some(&new_activity_id(&actor_id)),
        None,
        Some(vec![pubby_sdk::AS_PUBLIC.to_string()]),
        Some(vec![followers]),
        None,
    );
    delivery::publish(ap, &username, &undo).await?;
    Ok(())
}
