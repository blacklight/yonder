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
/// honouring `q=` weights and media-range specificity (`q=0` means
/// "not acceptable"). [`ContentKind::Any`] when nothing known is
/// preferred.
pub fn accept(headers: &HeaderMap) -> ContentKind {
    let Some(value) = headers.get(ACCEPT).and_then(|v| v.to_str().ok()) else {
        return ContentKind::Any;
    };
    let mut best: Option<(f32, bool, ContentKind)> = None;
    for entry in value.split(',') {
        let mut parts = entry.trim().split(';');
        let media = parts.next().unwrap_or("").trim();
        if media.is_empty() {
            continue;
        }
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0);
        if q <= 0.0 {
            continue; // q=0: client explicitly refuses this type
        }
        let (kind, specific) = match media.to_ascii_lowercase().as_str() {
            "application/activity+json" => (ContentKind::ActivityJson, true),
            "application/ld+json" => (ContentKind::LdJson, true),
            "text/html" => (ContentKind::Html, true),
            "application/json" => (ContentKind::Json, true),
            "*/*" => (ContentKind::Any, false),
            _ => continue,
        };
        // Higher q wins; on ties the more specific media range wins.
        let candidate = (q, specific, kind);
        if best
            .as_ref()
            .is_none_or(|b| candidate.0 > b.0 || (candidate.0 == b.0 && candidate.1 && !b.1))
        {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, kind)| kind).unwrap_or(ContentKind::Any)
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
