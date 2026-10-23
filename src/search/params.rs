//! Search query-string parsing and `q` classification — pure logic,
//! no I/O. Mirrors Mastodon's rules: `@user@domain` (or bare
//! `user@domain`) is an account lookup, `https://…` is a URL lookup,
//! `#tag` is a hashtag match, everything else is full text.

use std::collections::HashMap;

use crate::posts::mentions::plausible_domain;
use crate::{Error, Result};

/// Mastodon's loose booleans — `true`/`1` count, everything else
/// doesn't (same rule as `timelines::truthy`).
fn truthy(params: &HashMap<String, String>, key: &str) -> bool {
    params.get(key).is_some_and(|v| v == "true" || v == "1")
}

fn int(params: &HashMap<String, String>, key: &str) -> Option<i64> {
    params.get(key).and_then(|s| s.parse::<i64>().ok())
}

/// The `type` filter — which result bucket to populate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchType {
    Accounts,
    Statuses,
    Hashtags,
}

/// Parsed `/api/v{1,2}/search` parameters.
#[derive(Debug, Clone)]
pub struct SearchParams {
    /// The query, trimmed.
    pub q: String,
    /// `accounts` / `statuses` / `hashtags`; `None` searches all.
    pub r#type: Option<SearchType>,
    /// Attempt remote lookup (`resolve`) — honored only for
    /// authenticated callers when `search.remote_fetch` is on.
    pub resolve: bool,
    /// Restrict account results to accounts the caller follows.
    pub following: bool,
    /// Restrict status results to one author (`account_id`).
    pub account_id: Option<i64>,
    /// Offset pagination (search doesn't use cursors).
    pub offset: i64,
    /// Page size, clamped 1..=40 like `paginate::parse`.
    pub limit: i64,
}

/// What `q` actually is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryKind {
    /// `@user@domain` / `user@domain` — exact account lookup.
    Fqn { username: String, domain: String },
    /// `https://…` — URL lookup (local first, remote per gates).
    Url(String),
    /// `#tag` (or bare text with `type=hashtags`) — tag match.
    Hashtag(String),
    /// Everything else — full text over accounts, statuses, tags.
    Text(String),
}

/// Parse the request query string. `q` is required and non-blank.
pub fn parse(params: &HashMap<String, String>) -> Result<SearchParams> {
    let q = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if q.is_empty() {
        return Err(Error::Validation(
            "search requires a q parameter".to_string(),
        ));
    }
    if q.chars().count() > 512 {
        return Err(Error::Validation("search query too long".to_string()));
    }
    let r#type = match params.get("type").map(String::as_str) {
        None => None,
        Some("accounts") => Some(SearchType::Accounts),
        Some("statuses") => Some(SearchType::Statuses),
        Some("hashtags") => Some(SearchType::Hashtags),
        Some(other) => {
            return Err(Error::Validation(format!("invalid search type: {other}")));
        }
    };
    Ok(SearchParams {
        q,
        r#type,
        resolve: truthy(params, "resolve"),
        following: truthy(params, "following"),
        account_id: int(params, "account_id"),
        offset: int(params, "offset").unwrap_or(0).max(0),
        limit: int(params, "limit").unwrap_or(20).clamp(1, 40),
    })
}

/// Classify `q` into an operator kind — the same priority Mastodon
/// uses: URL first, then FQN, then explicit hashtag, then text.
pub fn classify(q: &str) -> QueryKind {
    let q = q.trim();
    if (q.starts_with("https://") || q.starts_with("http://")) && url::Url::parse(q).is_ok() {
        return QueryKind::Url(q.to_string());
    }
    // FQN: `@user@domain` or bare `user@domain` — the domain must look
    // like a host (same `plausible_domain` rule mentions use).
    let body = q.strip_prefix('@').unwrap_or(q);
    if let Some((username, domain)) = body.rsplit_once('@') {
        let username = username.trim_start_matches('@');
        if !username.is_empty()
            && username.chars().count() <= 100
            && username
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            && plausible_domain(domain)
        {
            return QueryKind::Fqn {
                username: username.to_string(),
                domain: domain.to_lowercase(),
            };
        }
    }
    if let Some(tag) = q.strip_prefix('#') {
        let tag = tag.trim();
        if !tag.is_empty() {
            return QueryKind::Hashtag(tag.to_lowercase());
        }
    }
    QueryKind::Text(q.to_string())
}

/// Autocomplete mode: the caller narrowed to one `type` and `q` is a
/// plain word — account and tag matching switch from substring to
/// prefix. FQN/URL lookups are never prefix-mode.
pub fn is_prefix_mode(p: &SearchParams, kind: &QueryKind) -> bool {
    p.r#type.is_some() && matches!(kind, QueryKind::Text(_) | QueryKind::Hashtag(_))
}

/// Effective kind: a bare word with `type=hashtags` searches tags.
pub fn effective_kind(p: &SearchParams, kind: QueryKind) -> QueryKind {
    match (kind, p.r#type) {
        (QueryKind::Text(t), Some(SearchType::Hashtags)) => QueryKind::Hashtag(t.to_lowercase()),
        (kind, _) => kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn missing_q_is_validation() {
        assert!(matches!(parse(&params(&[])), Err(Error::Validation(_))));
        assert!(matches!(
            parse(&params(&[("q", "   ")])),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn defaults_and_clamps() {
        let p = parse(&params(&[("q", "hi"), ("limit", "500"), ("offset", "-3")])).unwrap();
        assert_eq!(p.limit, 40);
        assert_eq!(p.offset, 0);
        assert!(!p.resolve);
        let p = parse(&params(&[
            ("q", "hi"),
            ("resolve", "1"),
            ("following", "true"),
        ]))
        .unwrap();
        assert!(p.resolve && p.following);
    }

    #[test]
    fn type_values() {
        assert!(matches!(
            parse(&params(&[("q", "x"), ("type", "accounts")]))
                .unwrap()
                .r#type,
            Some(SearchType::Accounts)
        ));
        assert!(matches!(
            parse(&params(&[("q", "x"), ("type", "bogus")])),
            Err(Error::Validation(_))
        ));
    }

    #[test]
    fn classify_url() {
        assert_eq!(
            classify("https://remote.example/@alice/123"),
            QueryKind::Url("https://remote.example/@alice/123".to_string())
        );
        assert!(matches!(classify("https:// not a url"), QueryKind::Text(_)));
        assert!(matches!(classify("ftp://host/x"), QueryKind::Text(_)));
    }

    #[test]
    fn classify_fqn() {
        assert_eq!(
            classify("@alice@remote.example"),
            QueryKind::Fqn {
                username: "alice".to_string(),
                domain: "remote.example".to_string()
            }
        );
        assert_eq!(
            classify("alice@remote.example"),
            QueryKind::Fqn {
                username: "alice".to_string(),
                domain: "remote.example".to_string()
            }
        );
    }

    #[test]
    fn classify_rejects_email_shape_and_bare_at() {
        // Single-dot TLD fails plausible_domain → text, not FQN.
        assert!(matches!(classify("a@b.c"), QueryKind::Text(_)));
        assert!(matches!(classify("a@b"), QueryKind::Text(_)));
        assert!(matches!(classify("@alice"), QueryKind::Text(_)));
    }

    #[test]
    fn classify_hashtag() {
        assert_eq!(classify("#Cats"), QueryKind::Hashtag("cats".to_string()));
        assert!(matches!(classify("#"), QueryKind::Text(_)));
    }

    #[test]
    fn effective_kind_maps_type_hashtags() {
        let p = parse(&params(&[("q", "cats"), ("type", "hashtags")])).unwrap();
        assert_eq!(
            effective_kind(&p, classify("cats")),
            QueryKind::Hashtag("cats".to_string())
        );
        let p = parse(&params(&[("q", "cats"), ("type", "accounts")])).unwrap();
        assert_eq!(
            effective_kind(&p, classify("cats")),
            QueryKind::Text("cats".to_string())
        );
    }

    #[test]
    fn prefix_mode_only_with_type_and_word() {
        let p = parse(&params(&[("q", "al"), ("type", "accounts")])).unwrap();
        assert!(is_prefix_mode(&p, &classify("al")));
        let p = parse(&params(&[("q", "al")])).unwrap();
        assert!(!is_prefix_mode(&p, &classify("al")));
        let p = parse(&params(&[("q", "@a@b.co"), ("type", "accounts")])).unwrap();
        assert!(!is_prefix_mode(&p, &classify("@a@b.co")));
    }
}
