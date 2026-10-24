//! The Mastodon `CustomEmoji` entity and the public list.

use serde_json::{json, Value};

use crate::{config::Config, media, Result};

use super::model::EmojiRow;

/// The public URL for a stored emoji — local key through
/// `media::public_url`, remote rows from `remote_url` (local wins
/// when both exist, matching the remote-media-cache convention).
pub fn emoji_url(config: &Config, base_url: &str, row: &EmojiRow) -> Option<String> {
    let local = row
        .image_path
        .as_ref()
        .map(|k| media::public_url(config, base_url, k));
    local.or_else(|| row.remote_url.clone())
}

/// The Mastodon `CustomEmoji` shape — `static_url` equals `url` until
/// animated variants land (follow-up).
pub fn emoji_entity(config: &Config, base_url: &str, row: &EmojiRow) -> Value {
    let url = emoji_url(config, base_url, row);
    json!({
        "shortcode": row.shortcode,
        "url": url,
        "static_url": url,
        "visible_in_picker": row.visible_in_picker,
        "category": row.category,
    })
}

/// `GET /api/v1/custom_emojis` — enabled, picker-visible local emoji.
pub async fn public_list(db: &sqlx::PgPool, config: &Config, base_url: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, shortcode, domain, image_path, remote_url, disabled, \
                visible_in_picker, category, pack_id \
         FROM custom_emojis \
         WHERE domain IS NULL AND NOT disabled AND visible_in_picker \
         ORDER BY shortcode",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| emoji_entity(config, base_url, &super::model::row_to_emoji(r)))
        .collect())
}
