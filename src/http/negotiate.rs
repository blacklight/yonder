//! Content negotiation for dual-purpose routes.
//!
//! The same URI can serve an ActivityPub document
//! (`application/activity+json`, `application/ld+json`) or a
//! human-readable page (`text/html`) — epic 06 dereference endpoints
//! and epic 18 profile pages pick their representation through
//! [`accept`].

use axum::http::{header::ACCEPT, HeaderMap};

/// The representation a client prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// `application/activity+json`.
    ActivityJson,
    /// `application/ld+json` (typically with an ActivityStreams
    /// `profile=` parameter).
    LdJson,
    /// `text/html` — a human page.
    Html,
    /// Plain `application/json`.
    Json,
    /// No usable preference (`*/*` or absent).
    Any,
}

/// Resolve the best-known content kind from the `Accept` header,
/// honouring `q=` weights. [`ContentKind::Any`] when nothing known is
/// preferred.
pub fn accept(headers: &HeaderMap) -> ContentKind {
    let Some(value) = headers.get(ACCEPT).and_then(|v| v.to_str().ok()) else {
        return ContentKind::Any;
    };
    let mut offers: Vec<(&str, f32)> = value
        .split(',')
        .filter_map(|entry| {
            let mut parts = entry.trim().split(';');
            let media = parts.next()?.trim();
            if media.is_empty() {
                return None;
            }
            let q = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|v| v.parse::<f32>().ok())
                .unwrap_or(1.0);
            Some((media, q))
        })
        .collect();
    offers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (media, _) in offers {
        let kind = match media {
            "application/activity+json" => ContentKind::ActivityJson,
            "application/ld+json" => ContentKind::LdJson,
            "text/html" => ContentKind::Html,
            "application/json" => ContentKind::Json,
            "*/*" => return ContentKind::Any,
            _ => continue,
        };
        return kind;
    }
    ContentKind::Any
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;

    fn headers(accept: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ACCEPT, HeaderValue::from_str(accept).unwrap());
        h
    }

    #[test]
    fn prefers_highest_q() {
        assert_eq!(
            accept(&headers("text/html;q=0.5, application/activity+json")),
            ContentKind::ActivityJson
        );
    }

    #[test]
    fn ld_json_with_profile() {
        assert_eq!(
            accept(&headers(
                r#"application/ld+json; profile="https://www.w3.org/ns/activitystreams""#
            )),
            ContentKind::LdJson
        );
    }

    #[test]
    fn browser_gets_html() {
        assert_eq!(
            accept(&headers(
                "text/html,application/xhtml+xml,image/avif,*/*;q=0.8"
            )),
            ContentKind::Html
        );
    }

    #[test]
    fn absent_or_wildcard_is_any() {
        assert_eq!(accept(&HeaderMap::new()), ContentKind::Any);
        assert_eq!(accept(&headers("*/*")), ContentKind::Any);
        assert_eq!(accept(&headers("image/png")), ContentKind::Any);
    }
}
