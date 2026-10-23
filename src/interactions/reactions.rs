//! Custom emoji reactions — Pleroma/Akkoma-style `emoji_reactions`
//! rows plus outbound `EmojiReact`/`Undo(EmojiReact)` (pubby has no
//! builder for the type, so the activity JSON is constructed here).

use serde_json::{json, Value};

use crate::{
    activitypub::{delivery, ApContext},
    http::middleware::auth::AuthContext,
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

use super::{
    account_parts, emit_notification_event, load_visible, new_activity_id, stored_activity,
};

/// A resolved reaction name: the stored `emoji` value plus an
/// optional `custom_emojis` link.
#[derive(Debug)]
pub struct ResolvedEmoji {
    /// The canonical stored name — `:shortcode:`-less custom emoji or
    /// the unicode grapheme.
    pub name: String,
    /// `custom_emojis.id` when the reaction is a known custom emoji.
    pub custom_emoji_id: Option<i64>,
}

/// Whether `name` looks like a custom-emoji shortcode
/// (`[a-zA-Z0-9_]+`, optionally `@domain`).
fn is_shortcode(name: &str) -> bool {
    let code = name.split('@').next().unwrap_or(name);
    !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Resolve the `custom_emojis` row for `code`/`domain` (domain =
/// remote host, `None` = local emoji).
pub async fn lookup_custom_emoji(
    db: &sqlx::PgPool,
    code: &str,
    domain: Option<&str>,
) -> Result<Option<i64>> {
    match domain {
        Some(d) => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM custom_emojis \
             WHERE lower(shortcode) = lower($1) AND domain = $2 AND NOT disabled LIMIT 1",
        )
        .bind(code)
        .bind(d)
        .fetch_optional(db)
        .await
        .map_err(Into::into),
        None => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM custom_emojis \
             WHERE lower(shortcode) = lower($1) AND domain IS NULL AND NOT disabled LIMIT 1",
        )
        .bind(code)
        .fetch_optional(db)
        .await
        .map_err(Into::into),
    }
}

/// Normalize an API-supplied `emoji` param to the stored name.
/// `:shortcode:`/`shortcode@domain` resolve through `custom_emojis`
/// (unknown → 422); anything else must be a short unicode grapheme
/// (a cheap heuristic — the allowlist is the emoji blocks, enforced
/// by shape rather than a table).
pub async fn resolve_emoji(ctx: &PostContext, raw: &str) -> Result<ResolvedEmoji> {
    let name = raw.trim().trim_matches(':').to_string();
    if name.is_empty() {
        return Err(Error::Validation("emoji cannot be empty".to_string()));
    }
    if is_shortcode(&name) {
        let (code, domain) = match name.split_once('@') {
            Some((c, d)) => (c, Some(d)),
            None => (name.as_str(), None),
        };
        let id = lookup_custom_emoji(&ctx.db, code, domain).await?;
        return id
            .map(|custom_emoji_id| ResolvedEmoji {
                name: name.clone(),
                custom_emoji_id: Some(custom_emoji_id),
            })
            .ok_or_else(|| Error::Validation(format!("unknown emoji :{name}:")));
    }
    if name.chars().count() > 8
        || name
            .chars()
            .any(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
    {
        return Err(Error::Validation(
            "emoji must be a unicode emoji or :shortcode:".to_string(),
        ));
    }
    Ok(ResolvedEmoji {
        name,
        custom_emoji_id: None,
    })
}

/// Lenient inbound variant — remote shortcodes we don't know are
/// still recorded (the emoji set lives on the sender's instance).
pub async fn parse_emoji_inbound(db: &sqlx::PgPool, raw: &str) -> Option<ResolvedEmoji> {
    let name = raw.trim().trim_matches(':').to_string();
    if name.is_empty() || name.chars().count() > 100 {
        return None;
    }
    if is_shortcode(&name) {
        let (code, domain) = match name.split_once('@') {
            Some((c, d)) => (c, Some(d)),
            None => (name.as_str(), None),
        };
        let custom_emoji_id = lookup_custom_emoji(db, code, domain).await.ok().flatten();
        return Some(ResolvedEmoji {
            name,
            custom_emoji_id,
        });
    }
    if name.chars().count() > 8 {
        return None;
    }
    Some(ResolvedEmoji {
        name,
        custom_emoji_id: None,
    })
}

/// `PUT|POST /api/v1/pleroma/statuses/{id}/reactions/{emoji}` —
/// idempotent per UNIQUE `(account_id, status_id, emoji)`.
pub async fn react(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
    emoji_raw: &str,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let emoji = resolve_emoji(ctx, emoji_raw).await?;
    let mut tx = ctx.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO emoji_reactions (account_id, status_id, emoji, custom_emoji_id) \
         VALUES ($1,$2,$3,$4) \
         ON CONFLICT (account_id, status_id, emoji) DO NOTHING",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .bind(&emoji.name)
    .bind(emoji.custom_emoji_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;

    // Publish only on a new row — replayed reactions don't mint a
    // second EmojiReact activity.
    if let (Some(ap), false, true) = (&ctx.ap, status.local, inserted > 0) {
        if let Err(e) = publish_emoji_react(ctx, ap, auth.account_id, &status, &emoji.name).await {
            tracing::error!(status_id = status.id, error = %e, "AP EmojiReact publish failed");
        }
    }
    if inserted > 0 {
        emit_notification_event(
            &ctx.db,
            &ctx.dispatcher,
            "emoji_reaction",
            auth.account_id,
            Some(status.id),
            status.account_id,
        )
        .await;
    }
    posts::load(&ctx.db, status.id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after react".into()))
}

/// `DELETE /api/v1/pleroma/statuses/{id}/reactions/{emoji}` —
/// idempotent.
pub async fn unreact(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
    emoji_raw: &str,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let emoji = resolve_emoji(ctx, emoji_raw).await?;
    let removed: Option<Option<String>> = sqlx::query_scalar(
        "DELETE FROM emoji_reactions \
         WHERE account_id = $1 AND status_id = $2 AND emoji = $3 \
         RETURNING activity_uri",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .bind(&emoji.name)
    .fetch_optional(&ctx.db)
    .await?;
    let activity_uri = removed.flatten();
    if let (Some(ap), Some(activity_uri)) = (&ctx.ap, activity_uri) {
        if let Err(e) = publish_undo_emoji_react(
            ctx,
            ap,
            auth.account_id,
            &status,
            &emoji.name,
            &activity_uri,
        )
        .await
        {
            tracing::error!(status_id = status.id, error = %e, "AP Undo(EmojiReact) publish failed");
        }
    }
    posts::load(&ctx.db, status.id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after unreact".into()))
}

/// `EmojiReact` activity JSON — `content` + `_misskey_reaction` carry
/// the emoji for the Pleroma/Akkoma/Misskey readers.
pub fn emoji_react_activity(actor_id: &str, object_uri: &str, emoji: &str, author: &str) -> Value {
    json!({
        "@context": pubby_sdk::model::ap_context(),
        "id": new_activity_id(actor_id),
        "type": "EmojiReact",
        "actor": actor_id,
        "object": object_uri,
        "content": emoji,
        "_misskey_reaction": emoji,
        "to": [author],
        "cc": [pubby_sdk::AS_PUBLIC],
    })
}

/// Deliver `EmojiReact` to the remote author; record the activity id.
async fn publish_emoji_react(
    ctx: &PostContext,
    ap: &ApContext,
    account_id: i64,
    status: &StatusRow,
    emoji: &str,
) -> Result<()> {
    let Some(uri) = status.uri.clone() else {
        return Ok(());
    };
    let (username, actor_id) = account_parts(ctx, account_id).await?;
    let (_, author_actor) = account_parts(ctx, status.account_id).await?;
    let activity = emoji_react_activity(&actor_id, &uri, emoji, &author_actor);
    let stored = delivery::publish(ap, &username, &activity).await?;
    if let Some(id) = stored.get("id").and_then(|v| v.as_str()) {
        sqlx::query(
            "UPDATE emoji_reactions SET activity_uri = $4 \
             WHERE account_id = $1 AND status_id = $2 AND emoji = $3",
        )
        .bind(account_id)
        .bind(status.id)
        .bind(emoji)
        .bind(id)
        .execute(&ctx.db)
        .await?;
    }
    Ok(())
}

/// `Undo(EmojiReact)` — inner rebuilt from `activities` or
/// deterministically.
async fn publish_undo_emoji_react(
    ctx: &PostContext,
    ap: &ApContext,
    account_id: i64,
    status: &StatusRow,
    emoji: &str,
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
            let mut a = emoji_react_activity(&actor_id, &uri, emoji, &author_actor);
            a["id"] = json!(activity_uri);
            a
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

/// Aggregated per-emoji reaction summary for the Status entity
/// (`pleroma.emoji_reactions`) and the reactions listing.
pub async fn reaction_summary(
    ctx: &PostContext,
    status_id: i64,
    viewer: Option<i64>,
    include_accounts: bool,
) -> Result<Vec<Value>> {
    let rows: Vec<(String, Option<i64>, i64, Option<bool>)> = sqlx::query_as(
        "SELECT emoji, custom_emoji_id, count(*)::bigint, \
                CASE WHEN $2::bigint IS NULL THEN NULL \
                     ELSE bool_or(account_id = $2) END AS me \
         FROM emoji_reactions WHERE status_id = $1 \
         GROUP BY emoji, custom_emoji_id ORDER BY min(created_at)",
    )
    .bind(status_id)
    .bind(viewer)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (emoji, custom_emoji_id, count, me) in rows {
        let mut entry = json!({
            "name": emoji,
            "count": count,
            "me": me.unwrap_or(false),
        });
        if let Some(ce) = custom_emoji_id {
            let url: Option<(Option<String>, Option<String>)> =
                sqlx::query_as("SELECT remote_url, image_path FROM custom_emojis WHERE id = $1")
                    .bind(ce)
                    .fetch_optional(&ctx.db)
                    .await?;
            if let Some((remote_url, image_path)) = url {
                entry["url"] = remote_url
                    .filter(|u| !u.is_empty())
                    .or_else(|| image_path.map(|k| crate::media::public_url(&ctx.base_url(), &k)))
                    .map(Value::String)
                    .unwrap_or(Value::Null);
            }
        }
        if include_accounts {
            let accounts: Vec<Value> = sqlx::query_scalar::<_, i64>(
                "SELECT account_id FROM emoji_reactions \
                 WHERE status_id = $1 AND emoji = $2 ORDER BY id",
            )
            .bind(status_id)
            .bind(&emoji)
            .fetch_all(&ctx.db)
            .await?
            .into_iter()
            .map(|id| json!(id.to_string()))
            .collect();
            entry["accounts"] = Value::Array(accounts);
        }
        out.push(entry);
    }
    Ok(out)
}
