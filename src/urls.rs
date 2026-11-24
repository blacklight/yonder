//! Shared guards for remote-controlled URL strings.
//!
//! Actor `url`s, status permalinks, media attachment URLs, quote
//! targets and preview-card links arrive verbatim from remote
//! ActivityPub documents and are stored as received — ingest can't
//! trust them, but the stored value is kept for provenance. Entity
//! rendering re-validates through [`safe_remote_url`] so a hostile or
//! broken document can never emit a non-http(s) URL into the API: a
//! `javascript:` permalink bound to `href`/`window.open` by a client
//! is a stored-XSS vector (the SPA keeps its session token in
//! `localStorage`).

use serde_json::Value;

/// The URL's serialization when `raw` is an absolute `http`/`https`
/// URL with a host; `None` for anything else (`javascript:`, `data:`,
/// relative paths, unparseable input). `mailto:` is deliberately not
/// allowed — these fields are permalink/profile/media links, not
/// contact addresses.
pub(crate) fn safe_remote_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(url.to_string())
}

/// [`safe_remote_url`] mapped into a JSON string, `null` when unsafe —
/// the shape entity fields take when the stored value can't be
/// emitted (Mastodon likewise never serves non-http(s) permalinks).
pub(crate) fn safe_url_json(raw: &str) -> Value {
    safe_remote_url(raw).map_or(Value::Null, Value::String)
}

/// [`safe_url_json`] for an optional stored value.
pub(crate) fn safe_opt_url_json(raw: Option<&str>) -> Value {
    raw.and_then(safe_remote_url)
        .map_or(Value::Null, Value::String)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn accepts_http_and_https() {
        assert_eq!(
            safe_remote_url("https://example.com/@alice").as_deref(),
            Some("https://example.com/@alice")
        );
        assert_eq!(
            safe_remote_url("http://example.com:8080/s/1").as_deref(),
            Some("http://example.com:8080/s/1")
        );
    }

    #[test]
    fn rejects_unsafe_and_hostless() {
        for raw in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "mailto:a@b.c",
            "ftp://example.com/x",
            "/relative/path",
            "not a url",
            "",
        ] {
            assert_eq!(safe_remote_url(raw), None, "{raw}");
        }
    }

    #[test]
    fn json_helpers_emit_null_when_unsafe() {
        assert_eq!(safe_url_json("javascript:alert(1)"), Value::Null);
        assert_eq!(
            safe_url_json("https://x.test/s/1"),
            json!("https://x.test/s/1")
        );
        assert_eq!(safe_opt_url_json(None), Value::Null);
        assert_eq!(
            safe_opt_url_json(Some("https://x.test")),
            json!("https://x.test/")
        );
    }
}
