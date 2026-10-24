//! Mastodon entity JSON — Status, ScheduledStatus, Poll and
//! MediaAttachment — plus the `yonder` extension block (raw `source`,
//! `content_type`, quote data) the PROMPT asks for.

use serde_json::{json, Value};
use sqlx::Row;

use crate::{media, users::account as account_mapper, Result};

use super::{polls, wire_visibility, PostContext, StatusRow};

/// A `media_attachments` row.
#[derive(Debug)]
pub struct MediaRow {
    pub id: i64,
    pub account_id: i64,
    pub status_id: Option<i64>,
    pub kind: String,
    pub file_path: Option<String>,
    pub remote_url: Option<String>,
    pub content_type: Option<String>,
    pub file_size: Option<i64>,
    pub description: Option<String>,
    pub blurhash: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub focus_x: Option<f32>,
    pub focus_y: Option<f32>,
    pub processed: bool,
}

fn media_from_row(r: &sqlx::postgres::PgRow) -> MediaRow {
    MediaRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        status_id: r.get("status_id"),
        kind: r.get("type"),
        file_path: r.get("file_path"),
        remote_url: r.get("remote_url"),
        content_type: r.get("content_type"),
        file_size: r.get("file_size"),
        description: r.get("description"),
        blurhash: r.get("blurhash"),
        width: r.get("width"),
        height: r.get("height"),
        focus_x: r.get("focus_x"),
        focus_y: r.get("focus_y"),
        processed: r.get("processed"),
    }
}

const MEDIA_COLS: &str = "id, account_id, status_id, type, file_path, remote_url, content_type, \
     file_size, description, blurhash, width, height, focus_x, focus_y, processed";

/// Load one media row by id.
pub async fn load_media(ctx: &PostContext, media_id: i64) -> Result<Option<MediaRow>> {
    let row = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments WHERE id = $1"
    ))
    .bind(media_id)
    .fetch_optional(&ctx.db)
    .await?;
    Ok(row.as_ref().map(media_from_row))
}

/// Load every media row attached to a status.
pub async fn media_for_status(ctx: &PostContext, status_id: i64) -> Result<Vec<MediaRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments WHERE status_id = $1 ORDER BY id"
    ))
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows.iter().map(media_from_row).collect())
}

/// Media rows reserved by a scheduled status.
pub async fn media_for_scheduled(ctx: &PostContext, scheduled_id: i64) -> Result<Vec<MediaRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments m \
         JOIN scheduled_status_media sm ON sm.media_attachment_id = m.id \
         WHERE sm.scheduled_status_id = $1 ORDER BY m.id"
    ))
    .bind(scheduled_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows.iter().map(media_from_row).collect())
}

/// The public URL for a stored attachment (local key or remote URL).
pub fn media_url(config: &crate::config::Config, base_url: &str, row: &MediaRow) -> Option<String> {
    let local = row
        .file_path
        .as_ref()
        .map(|k| media::public_url(config, base_url, k));
    let remote = row.remote_url.clone();
    // Remote-media caching on → the local copy wins; off → the remote
    // URL is served directly.
    if config.media.fetch_remote_media {
        local.or(remote)
    } else {
        remote.or(local)
    }
}

/// The Mastodon MediaAttachment entity: `url`/`preview_url` resolve
/// the stored key to a public URL; `meta` carries dimensions and the
/// `focus` point when set.
pub fn media_entity(config: &crate::config::Config, base_url: &str, row: &MediaRow) -> Value {
    let url = media_url(config, base_url, row);
    let mut meta = json!({});
    if let (Some(w), Some(h)) = (row.width, row.height) {
        meta["original"] = json!({
            "width": w,
            "height": h,
            "size": format!("{w}x{h}"),
            "aspect": w as f64 / h.max(1) as f64,
        });
    }
    if let (Some(x), Some(y)) = (row.focus_x, row.focus_y) {
        meta["focus"] = json!({ "x": x, "y": y });
    }
    json!({
        "id": row.id.to_string(),
        "type": row.kind,
        "url": url,
        "preview_url": url,
        "remote_url": row.remote_url,
        "description": row.description,
        "blurhash": row.blurhash,
        "meta": meta,
        "yonder": {
            "content_type": row.content_type,
            "file_size": row.file_size,
            "processed": row.processed,
        },
    })
}

/// The `mentions[]` entries for a status (`{id, username, url, acct}`).
async fn mention_entities(ctx: &PostContext, status_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT a.id, a.username, a.domain, a.url, a.actor_id \
         FROM status_mentions sm JOIN accounts a ON a.id = sm.account_id \
         WHERE sm.status_id = $1 ORDER BY a.id",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let username: String = r.get("username");
            let domain: Option<String> = r.get("domain");
            let url: Option<String> = r.get("url");
            let actor_id: String = r.get("actor_id");
            json!({
                "id": r.get::<i64, _>("id").to_string(),
                "username": username,
                "acct": match &domain {
                    Some(d) => format!("{username}@{d}"),
                    None => username.clone(),
                },
                "url": url.filter(|u| !u.is_empty()).unwrap_or(actor_id),
            })
        })
        .collect())
}

/// The `tags[]` entries (`{name, url}`) for a status.
async fn tag_entities(ctx: &PostContext, status_id: i64, base_url: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
         WHERE st.status_id = $1 ORDER BY t.name",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "url": format!("{}/tags/{name}", base_url.trim_end_matches('/')),
            })
        })
        .collect())
}

/// The `card` (link preview) entity when a `preview_cards` row exists.
async fn card_entity(ctx: &PostContext, status_id: i64, base_url: &str) -> Result<Value> {
    let row = sqlx::query(
        "SELECT url, title, description, image_path, author_name, author_url, \
                provider_name, provider_url FROM preview_cards WHERE status_id = $1 \
         ORDER BY id LIMIT 1",
    )
    .bind(status_id)
    .fetch_optional(&ctx.db)
    .await?;
    Ok(match row {
        Some(r) => {
            let image: Option<String> = r.get("image_path");
            json!({
                "url": r.get::<String, _>("url"),
                "title": r.get::<String, _>("title"),
                "description": r.get::<String, _>("description"),
                "type": "link",
                "author_name": r.get::<String, _>("author_name"),
                "author_url": r.get::<String, _>("author_url"),
                "provider_name": r.get::<String, _>("provider_name"),
                "provider_url": r.get::<String, _>("provider_url"),
                "image": image.map(|k| media::public_url(&ctx.config, base_url, &k)),
                "blurhash": Value::Null,
            })
        }
        None => Value::Null,
    })
}

/// The Mastodon Status entity. `content` serves the sanitized
/// `content_html` (falling back to escaped `content` for rows that
/// predate the column); the `yonder` block carries the raw `source`,
/// `content_type` and quote data.
pub async fn status_entity(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
) -> Result<Value> {
    status_entity_inner(ctx, status, viewer, 0).await
}

/// `depth` bounds reblog/quote nesting to one level.
/// `pub(crate)` — `interactions::entity::quote_json` recurses through
/// it with `depth + 1`.
pub(crate) async fn status_entity_inner(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
    depth: u8,
) -> Result<Value> {
    let base_url = ctx.base_url();
    let account = account_mapper::load(&ctx.db, status.account_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("account".into()))?;
    let mut account_json = account_mapper::to_account(&account, &base_url, &ctx.config);
    account_json["moved"] =
        crate::users::moves::moved_account_json(&ctx.db, &account, &base_url, &ctx.config).await;

    // Epic 20 — one domain-scoped emoji map shared by the author
    // entity, the content and the `emojis` array: remote authors
    // resolve `:code:` against their own domain's recorded emoji.
    // Skip the query entirely when no text could carry a shortcode.
    let mut texts: Vec<&str> = vec![
        status.content.as_str(),
        status.spoiler_text.as_deref().unwrap_or_default(),
    ];
    texts.extend(account_mapper::emoji_texts(&account));
    let emoji_map = crate::emoji::render::EmojiMap::for_author_texts(
        &ctx.db,
        &ctx.config,
        &base_url,
        account.domain.as_deref(),
        &texts,
    )
    .await?;
    crate::emoji::render::apply_account_with_map(&mut account_json, &emoji_map);
    let media_rows = media_for_status(ctx, status.id).await?;
    let media_json: Vec<Value> = media_rows
        .iter()
        .map(|m| media_entity(&ctx.config, &base_url, m))
        .collect();
    let poll_json = match status.poll_id {
        Some(poll_id) => polls::entity(ctx, poll_id, viewer).await?,
        None => Value::Null,
    };
    let content = status
        .content_html
        .clone()
        .unwrap_or_else(|| pubby_sdk::content::html_escape(&status.content));
    // Interaction state (epic 13): viewer flags, reblog nesting,
    // the Mastodon-4.4 quote block and Pleroma reaction summary.
    let flags = crate::interactions::entity::viewer_flags(ctx, status, viewer).await?;
    let quote_block = crate::interactions::entity::quote_json(ctx, status, viewer, depth).await?;
    let emoji_reactions =
        crate::interactions::reactions::reaction_summary(ctx, status.id, viewer, false).await?;
    let (reblog, content, spoiler, media_json, poll_json) = match (status.reblog_of_id, depth) {
        (Some(orig), 0) => {
            let inner = super::load(&ctx.db, orig).await?;
            let nested = match &inner {
                Some(s) if super::can_see(&ctx.db, s, viewer).await? => {
                    Box::pin(status_entity_inner(ctx, s, viewer, 1)).await?
                }
                _ => Value::Null,
            };
            // Mastodon renders "" for the boost wrapper's own text.
            (
                nested,
                Value::String(String::new()),
                Value::String(String::new()),
                Vec::new(),
                Value::Null,
            )
        }
        _ => (
            Value::Null,
            Value::String(crate::emoji::render::substitute(&content, &emoji_map)),
            Value::String(crate::emoji::render::substitute(
                &status.spoiler_text.clone().unwrap_or_default(),
                &emoji_map,
            )),
            media_json,
            poll_json,
        ),
    };
    Ok(json!({
        "id": status.id.to_string(),
        "uri": status.uri,
        "url": status.url,
        "account": account_json,
        "content": content,
        "content_type": status.content_type,
        "created_at": status.created_at.to_rfc3339(),
        "edited_at": status.edited_at.map(|e| e.to_rfc3339()),
        "visibility": wire_visibility(&status.visibility),
        "sensitive": status.sensitive,
        "spoiler_text": spoiler,
        "media_attachments": media_json,
        "mentions": mention_entities(ctx, status.id).await?,
        "tags": tag_entities(ctx, status.id, &base_url).await?,
        "emojis": emoji_map.entities_in(&format!(
            "{} {}",
            status.content,
            status.spoiler_text.clone().unwrap_or_default()
        )),
        "card": card_entity(ctx, status.id, &base_url).await?,
        "poll": poll_json,
        "application": Value::Null,
        "language": status.language,
        "in_reply_to_id": status.in_reply_to_id.map(|i| i.to_string()),
        "in_reply_to_account_id": status.in_reply_to_account_id.map(|i| i.to_string()),
        "reblog": reblog,
        "replies_count": status.replies_count,
        "reblogs_count": status.reblogs_count,
        "favourites_count": status.favourites_count,
        "quotes_count": status.quotes_count,
        "quote_id": quote_block.get("quote_id").cloned().unwrap_or(Value::Null),
        "quote": quote_block.get("quote").cloned().unwrap_or(Value::Null),
        "reblogged": flags["reblogged"],
        "favourited": flags["favourited"],
        "bookmarked": flags["bookmarked"],
        "pinned": flags["pinned"],
        "muted": false,
        "pleroma": {
            "emoji_reactions": emoji_reactions,
        },
        "yonder": {
            "source": status.content,
            "content_type": status.content_type,
            "quote": match (status.quote_of_id, &status.quote_uri) {
                (Some(id), _) => json!({"id": id.to_string(), "uri": quote_block.get("yonder_quote_uri").cloned().unwrap_or(Value::Null)}),
                (None, Some(uri)) => json!({"id": Value::Null, "uri": uri}),
                _ => Value::Null,
            },
        },
    }))
}

/// A `scheduled_statuses` row.
#[derive(Debug)]
pub struct ScheduledRow {
    pub id: i64,
    pub account_id: i64,
    pub scheduled_at: chrono::DateTime<chrono::Utc>,
    pub params: Value,
}

/// The Mastodon ScheduledStatus entity — media renders as full
/// MediaAttachment entities from the `scheduled_status_media` join.
pub async fn scheduled_status_entity(ctx: &PostContext, row: &ScheduledRow) -> Result<Value> {
    let base_url = ctx.base_url();
    let media_json: Vec<Value> = media_for_scheduled(ctx, row.id)
        .await?
        .iter()
        .map(|m| media_entity(&ctx.config, &base_url, m))
        .collect();
    Ok(json!({
        "id": row.id.to_string(),
        "scheduled_at": row.scheduled_at.to_rfc3339(),
        "params": row.params,
        "media_attachments": media_json,
    }))
}
