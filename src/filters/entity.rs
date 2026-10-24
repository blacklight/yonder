//! Mastodon v2 Filter / FilterKeyword / FilterStatus entities.

use serde_json::{json, Value};

use super::model::{FilterRow, KeywordRow};

/// The Filter entity — `filter_action` is the wire name for the
/// stored `action`. `statuses` renders `(pin_id, status_id)` pairs.
pub fn filter_entity(row: &FilterRow, keywords: &[KeywordRow], statuses: &[(i64, i64)]) -> Value {
    json!({
        "id": row.id.to_string(),
        "title": row.title,
        "context": row.context,
        "expires_at": row.expires_at.map(|e| e.to_rfc3339()),
        "filter_action": row.action,
        "keywords": keywords.iter().map(keyword_entity).collect::<Vec<_>>(),
        "statuses": statuses.iter().map(|(pin, sid)| filter_status_entity(*pin, *sid)).collect::<Vec<_>>(),
    })
}

/// The FilterKeyword entity.
pub fn keyword_entity(row: &KeywordRow) -> Value {
    json!({
        "id": row.id.to_string(),
        "keyword": row.keyword,
        "whole_word": row.whole_word,
    })
}

/// The FilterStatus entity — `id` is the pin row, `status_id` the
/// pinned status.
pub fn filter_status_entity(pin_id: i64, status_id: i64) -> Value {
    json!({
        "id": pin_id.to_string(),
        "status_id": status_id.to_string(),
    })
}
