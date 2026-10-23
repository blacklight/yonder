//! Mastodon-style `max_id`/`min_id`/`limit` pagination — parsed from
//! query params and emitted as `Link` headers (epic 13; timelines and
//! follow lists reuse it).
//!
//! Cursor semantics: list endpoints return newest-first rows with a
//! cursor column (the row id). `max_id` pages older (`id < max_id`),
//! `min_id`/`since_id` page newer (`id > min_id`). `limit` defaults
//! to 20, capped at 40.

use std::collections::HashMap;

use axum::http::{header, HeaderMap, HeaderValue, Uri};

/// The parsed pagination params.
#[derive(Debug, Clone, Default)]
pub struct Page {
    /// Only rows older than this cursor (`id < max_id`).
    pub max_id: Option<i64>,
    /// Only rows newer than this cursor (`id > since_id`) — `min_id`
    /// and `since_id` map to the same filter.
    pub since_id: Option<i64>,
    /// Page size (default 20, clamped to 1..=40).
    pub limit: i64,
}

/// Parse `max_id`/`min_id`/`since_id`/`limit` from query params.
pub fn parse(params: &HashMap<String, String>) -> Page {
    let int = |key: &str| params.get(key).and_then(|s| s.parse::<i64>().ok());
    Page {
        max_id: int("max_id"),
        since_id: int("min_id").or_else(|| int("since_id")),
        limit: int("limit").unwrap_or(20).clamp(1, 40),
    }
}

/// Build the `Link` header for a page of results: `rel="next"` when
/// `next_cursor` is set (older rows exist), always `rel="prev"` when
/// `prev_cursor` is set (newer rows). `uri`'s path is preserved; the
/// caller's other query params survive via `params`.
pub fn link_header(
    uri: &Uri,
    params: &HashMap<String, String>,
    next_cursor: Option<i64>,
    prev_cursor: Option<i64>,
) -> Option<HeaderValue> {
    let mut parts = Vec::new();
    let build = |cursor_key: &str, cursor: i64| {
        let mut q: Vec<(String, String)> = params
            .iter()
            .filter(|(k, _)| {
                k.as_str() != "max_id" && k.as_str() != "min_id" && k.as_str() != "since_id"
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        q.push((cursor_key.to_string(), cursor.to_string()));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(q.iter())
            .finish();
        format!("{}?{}", uri.path(), query)
    };
    if let Some(c) = next_cursor {
        parts.push(format!("<{}>; rel=\"next\"", build("max_id", c)));
    }
    if let Some(c) = prev_cursor {
        parts.push(format!("<{}>; rel=\"prev\"", build("min_id", c)));
    }
    if parts.is_empty() {
        return None;
    }
    HeaderValue::from_str(&parts.join(", ")).ok()
}

/// Emit `Link` when a page is full — the standard Mastodon contract:
/// `next` from the last cursor (only when the page was full), `prev`
/// from the first cursor (always, unless the caller opts out).
pub fn headers(
    uri: &Uri,
    params: &HashMap<String, String>,
    cursors: &[i64],
    limit: i64,
) -> HeaderMap {
    let mut map = HeaderMap::new();
    let next = if cursors.len() as i64 >= limit {
        cursors.last().copied()
    } else {
        None
    };
    let prev = cursors.first().copied();
    if let Some(v) = link_header(uri, params, next, prev) {
        map.insert(header::LINK, v);
    }
    map
}
