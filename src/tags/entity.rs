//! The Mastodon `Tag` entity shared by lookup, search and trends.

use serde_json::{json, Value};

use super::TagInfo;

/// One `history[]` bucket — `day` is a unix-day timestamp and `uses`
/// `accounts` stringified counts (Mastodon's wire shape).
#[derive(Debug, Clone)]
pub struct HistoryDay {
    pub day: i64,
    pub uses: i64,
    pub accounts: i64,
}

/// Render a tag: `id` (string), `name`, `url` (`{base}/tags/{name}`),
/// `history` newest-first, `following`, `featured` (Yonder has no
/// featured-tags surface — always `false`).
pub fn tag_json(base_url: &str, info: &TagInfo) -> Value {
    json!({
        "id": info.id.to_string(),
        "name": info.name,
        "url": format!("{}/tags/{}", base_url.trim_end_matches('/'), info.name),
        "history": info.history.iter().map(|h| json!({
            "day": h.day.to_string(),
            "uses": h.uses.to_string(),
            "accounts": h.accounts.to_string(),
        })).collect::<Vec<_>>(),
        "following": info.following,
        "featured": false,
    })
}
