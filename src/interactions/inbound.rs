//! Inbound interaction activities — `Like`, `Announce`, `EmojiReact`
//! and their `Undo`s, wired into `activitypub::incoming::on_activity_fn`
//! (the hook runs after signature verification, before pubby's
//! dispatch).
//!
//! `Like`/`Announce` and their `Undo`s answer [`PassThrough`] so
//! pubby's generic `interactions`-table bookkeeping (and the
//! `on_interaction_received` → `NotificationFanout` fan-out it
//! already drives) keeps running. `EmojiReact` — unknown to pubby's
//! `ActivityType` — is consumed here (`Handled`), including
//! `Undo(EmojiReact)` which pubby's Undo arm would silently drop.
//! `Accept` answers carrying a `QuoteAuthorization` (the reply to our
//! outbound FEP-044f `QuoteRequest`) are recorded and `Handled`.
//!
//! Guards: every arm only touches *local* statuses (remote-to-remote
//! interactions are not our business) and actors blocked by the
//! target's author are dropped silently.

use std::sync::Arc;

use pubby_sdk::{Activity, ActivityDisposition, ActivityPubStorage};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::activitypub::{actors::sync_block, ApCore};
use crate::Result;

use super::{is_blocked, object_uri};

/// Entry point from `on_activity_fn`. Errors log and pass through —
/// a broken side-effect must not drop the activity.
pub fn handle(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
) -> pubby_sdk::Result<ActivityDisposition> {
    let outcome = sync_block(inner(core, activity, raw));
    match outcome {
        Ok(d) => Ok(d),
        Err(e) => {
            tracing::warn!(
                error = %e,
                activity_type = %activity.activity_type,
                "inbound interaction handling failed; passing through"
            );
            Ok(ActivityDisposition::PassThrough)
        }
    }
}

async fn inner(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
) -> Result<ActivityDisposition> {
    match activity.activity_type.as_str() {
        "Like" => {
            like_inbound(core, activity, raw).await?;
            Ok(ActivityDisposition::PassThrough)
        }
        "Announce" => {
            announce_inbound(core, activity, raw).await?;
            Ok(ActivityDisposition::PassThrough)
        }
        "EmojiReact" => {
            react_inbound(core, activity, raw).await?;
            Ok(ActivityDisposition::Handled)
        }
        "Undo" => undo_inbound(core, activity, raw).await,
        "Accept" => accept_inbound(core, raw).await,
        _ => Ok(ActivityDisposition::PassThrough),
    }
}

/// The local status `uri` resolves to — `None` for remote targets,
/// tombstones and unknown objects (all no-ops).
async fn local_target(core: &ApCore, uri: &str) -> Result<Option<crate::posts::StatusRow>> {
    let status = crate::posts::load_by_uri(&core.db, uri).await?;
    Ok(status.filter(|s| s.local && !s.tombstone))
}

/// The remote actor's `accounts` row id via the epic-12 upsert;
/// `None` when the row can't be resolved.
async fn ensure_actor_account(core: &ApCore, actor_url: &str) -> Result<Option<i64>> {
    match crate::posts::remote::ensure_account(core, actor_url).await {
        Ok(id) => Ok(Some(id)),
        Err(e) => {
            tracing::warn!(error = %e, actor = %actor_url, "actor account upsert failed");
            Ok(None)
        }
    }
}

/// Whether the target's author blocks the actor — blocked inbound
/// interactions drop silently.
async fn blocked_on_target(db: &PgPool, target_author: i64, actor: i64) -> Result<bool> {
    is_blocked(db, target_author, actor).await
}

async fn like_inbound(core: &ApCore, activity: &Activity, raw: &Value) -> Result<()> {
    let Some(uri) = object_uri(raw.get("object")) else {
        return Ok(());
    };
    let Some(target) = local_target(core, &uri).await? else {
        return Ok(());
    };
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    if blocked_on_target(&core.db, target.account_id, actor).await? {
        return Ok(());
    }
    let mut tx = core.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO favourites (account_id, status_id) VALUES ($1,$2) \
         ON CONFLICT (account_id, status_id) DO NOTHING",
    )
    .bind(actor)
    .bind(target.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted > 0 {
        sqlx::query("UPDATE statuses SET favourites_count = favourites_count + 1 WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn announce_inbound(core: &ApCore, activity: &Activity, raw: &Value) -> Result<()> {
    let Some(uri) = object_uri(raw.get("object")) else {
        return Ok(());
    };
    let Some(target) = local_target(core, &uri).await? else {
        return Ok(());
    };
    // Reblog-of-reblog normalizes to the original.
    let target = match target.reblog_of_id {
        Some(orig) => crate::posts::load(&core.db, orig)
            .await?
            .filter(|s| s.local && !s.tombstone)
            .unwrap_or(target),
        None => target,
    };
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    if blocked_on_target(&core.db, target.account_id, actor).await? {
        return Ok(());
    }
    let mut tx = core.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO statuses (account_id, content, content_html, content_type, visibility, \
             local, reblog_of_id, uri, url) \
         VALUES ($1,'','','text/plain','public',false,$2,$3,$4) \
         ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL \
         DO NOTHING RETURNING id",
    )
    .bind(actor)
    .bind(target.id)
    .bind(if activity.id.is_empty() {
        None
    } else {
        Some(activity.id.clone())
    })
    .bind(&uri)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted > 0 {
        sqlx::query("UPDATE statuses SET reblogs_count = reblogs_count + 1 WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn undo_inbound(
    core: &ApCore,
    activity: &Activity,
    raw: &Value,
) -> Result<ActivityDisposition> {
    let inner = raw.get("object").cloned().unwrap_or(Value::Null);
    let inner_type = inner
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match inner_type {
        "Like" => {
            undo_like_inbound(core, activity, &inner).await?;
            Ok(ActivityDisposition::PassThrough)
        }
        "Announce" => {
            undo_announce_inbound(core, activity, &inner).await?;
            Ok(ActivityDisposition::PassThrough)
        }
        "EmojiReact" => {
            undo_react_inbound(core, activity, &inner).await?;
            Ok(ActivityDisposition::Handled)
        }
        _ => Ok(ActivityDisposition::PassThrough),
    }
}

async fn undo_like_inbound(core: &ApCore, activity: &Activity, inner: &Value) -> Result<()> {
    let Some(uri) = object_uri(inner.get("object")) else {
        return Ok(());
    };
    let Some(target) = local_target(core, &uri).await? else {
        return Ok(());
    };
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    let mut tx = core.db.begin().await?;
    let removed = sqlx::query("DELETE FROM favourites WHERE account_id = $1 AND status_id = $2")
        .bind(actor)
        .bind(target.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if removed > 0 {
        sqlx::query(
            "UPDATE statuses SET favourites_count = GREATEST(favourites_count - 1, 0) WHERE id = $1",
        )
        .bind(target.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn undo_announce_inbound(core: &ApCore, activity: &Activity, inner: &Value) -> Result<()> {
    let inner_id = inner.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    // Prefer the boost row's uri (the stored Announce id); fall back
    // to the (actor, target) pair.
    let boost_id: Option<i64> = if !inner_id.is_empty() {
        sqlx::query_scalar(
            "SELECT id FROM statuses WHERE account_id = $1 AND uri = $2 AND NOT tombstone",
        )
        .bind(actor)
        .bind(inner_id)
        .fetch_optional(&core.db)
        .await?
    } else {
        None
    };
    let boost_id = match boost_id {
        Some(id) => Some(id),
        None => match object_uri(inner.get("object")) {
            Some(uri) => match local_target(core, &uri).await? {
                Some(target) => {
                    sqlx::query_scalar(
                        "SELECT id FROM statuses \
                         WHERE account_id = $1 AND reblog_of_id = $2 AND NOT tombstone",
                    )
                    .bind(actor)
                    .bind(target.id)
                    .fetch_optional(&core.db)
                    .await?
                }
                None => None,
            },
            None => None,
        },
    };
    let Some(boost_id) = boost_id else {
        return Ok(());
    };
    let boost = crate::posts::load_any(&core.db, boost_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("boost".into()))?;
    let mut tx = core.db.begin().await?;
    // Decrement only when the tombstone transition wins — a duplicate
    // or racing `Undo` must not double-count.
    let tombstoned = sqlx::query(
        "UPDATE statuses SET tombstone = true, updated_at = now() \
         WHERE id = $1 AND NOT tombstone",
    )
    .bind(boost.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if tombstoned > 0 {
        if let Some(target_id) = boost.reblog_of_id {
            sqlx::query(
                "UPDATE statuses SET reblogs_count = GREATEST(reblogs_count - 1, 0) WHERE id = $1",
            )
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn react_inbound(core: &ApCore, activity: &Activity, raw: &Value) -> Result<()> {
    let Some(uri) = object_uri(raw.get("object")) else {
        return Ok(());
    };
    let Some(target) = local_target(core, &uri).await? else {
        return Ok(());
    };
    let emoji_raw = raw
        .get("content")
        .or_else(|| raw.get("_misskey_reaction"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let Some(emoji) = super::reactions::parse_emoji_inbound(&core.db, emoji_raw).await else {
        return Ok(());
    };
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    if blocked_on_target(&core.db, target.account_id, actor).await? {
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO emoji_reactions (account_id, status_id, emoji, custom_emoji_id) \
         VALUES ($1,$2,$3,$4) \
         ON CONFLICT (account_id, status_id, emoji) DO NOTHING",
    )
    .bind(actor)
    .bind(target.id)
    .bind(&emoji.name)
    .bind(emoji.custom_emoji_id)
    .execute(&core.db)
    .await?
    .rows_affected();
    // Pubby never sees EmojiReact, so the notification event is
    // ours to emit — but only once: a replayed EmojiReact is a no-op.
    if inserted > 0 {
        super::emit_notification_event(
            &core.db,
            &core.dispatcher,
            "emoji_reaction",
            actor,
            target.id,
            target.account_id,
        )
        .await;
    }
    Ok(())
}

async fn undo_react_inbound(core: &ApCore, activity: &Activity, inner: &Value) -> Result<()> {
    let Some(uri) = object_uri(inner.get("object")) else {
        return Ok(());
    };
    let Some(target) = local_target(core, &uri).await? else {
        return Ok(());
    };
    let emoji_raw = inner
        .get("content")
        .or_else(|| inner.get("_misskey_reaction"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let Some(emoji) = super::reactions::parse_emoji_inbound(&core.db, emoji_raw).await else {
        return Ok(());
    };
    let Some(actor) = ensure_actor_account(core, &activity.actor).await? else {
        return Ok(());
    };
    sqlx::query(
        "DELETE FROM emoji_reactions \
         WHERE account_id = $1 AND status_id = $2 AND emoji = $3",
    )
    .bind(actor)
    .bind(target.id)
    .bind(&emoji.name)
    .execute(&core.db)
    .await?;
    Ok(())
}

/// `Accept` carrying a `QuoteAuthorization` `result` — the reply to
/// our outbound `QuoteRequest`. Recorded in `quote_authorizations`
/// (future consent-gating reads it); no quote is withheld today.
async fn accept_inbound(core: &ApCore, raw: &Value) -> Result<ActivityDisposition> {
    let object = raw.get("object").cloned().unwrap_or(Value::Null);
    let is_quote_accept = matches!(
        object.get("type").and_then(|v| v.as_str()),
        Some("QuoteRequest")
    );
    if !is_quote_accept {
        return Ok(ActivityDisposition::PassThrough);
    }
    let Some(result_id) = raw
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Ok(ActivityDisposition::Handled);
    };
    let doc = json!({
        "id": result_id,
        "type": "QuoteAuthorization",
        "seen_in_accept": raw.get("id").and_then(|v| v.as_str()),
    });
    core.storage
        .store_quote_authorization(&result_id, &doc)
        .map_err(crate::Error::Federation)?;
    Ok(ActivityDisposition::Handled)
}
