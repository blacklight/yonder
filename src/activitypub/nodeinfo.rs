//! Remote instance software detection via NodeInfo — used by the
//! event RSVP protocol selection (plan.md §7.3): Friendica/Hubzilla
//! answer `Accept`/`Reject` on the event itself, everyone else takes
//! `Join`.
//!
//! `software_of(host)` fetches `https://{host}/.well-known/nodeinfo`,
//! follows the `links[]` entry with rel
//! `http://nodeinfo.diaspora.software/ns/doc/2.0` (falling back to any
//! `…/ns/doc/` link), and reads `software.name`. Fetches go through
//! pubby's SSRF-guarded [`HttpClient`] — unsigned is fine, NodeInfo
//! documents are public. Results cache in Redis: 24 h positive,
//! 1 h negative.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cache::RedisCache;

use super::{actors::sync_call, ApCore};

const POSITIVE_TTL: Duration = Duration::from_secs(24 * 3600);
const NEGATIVE_TTL: Duration = Duration::from_secs(3600);

fn cache_key(host: &str) -> String {
    format!("nodeinfo:software:{host}")
}

/// The software name a host's NodeInfo advertises, or `None` when it
/// can't be determined (fetch failed, no doc link, no `software.name`).
/// Cached — repeat calls for the same host are Redis reads.
pub async fn software_of(core: &ApCore, host: &str) -> Option<String> {
    let cache = RedisCache::new(core.redis.clone());
    let key = cache_key(host);
    if let Ok(Some(cached)) = cache.get(&key).await {
        // `{"s": null}` is the negative entry.
        return cached.get("s").and_then(|v| v.as_str()).map(str::to_string);
    }
    let name = fetch_software(core, host).await;
    let value = json!({"s": name});
    let ttl = if name.is_some() {
        POSITIVE_TTL
    } else {
        NEGATIVE_TTL
    };
    let _ = cache.set(&key, &value, ttl).await;
    name
}

/// One SSRF-guarded GET → parsed JSON, or `None` on any failure.
async fn fetch_json(core: &ApCore, url: &str) -> Option<Value> {
    let url = url.to_string();
    let timeout = Duration::from_secs(core.config.federation.request_timeout_seconds);
    let relaxed = core.config.federation.allow_private_targets;
    sync_call(move || {
        let http = if relaxed {
            pubby_sdk::http::HttpClient::new_relaxed(timeout)
        } else {
            pubby_sdk::http::HttpClient::new(timeout)
        };
        let headers: HashMap<String, String> = [(
            "Accept".to_string(),
            "application/json, application/activity+json".to_string(),
        )]
        .into_iter()
        .collect();
        let resp = http.get(&url, &headers).ok()?;
        if !resp.is_success() {
            return None;
        }
        resp.json().ok()
    })
    .await
}

async fn fetch_software(core: &ApCore, host: &str) -> Option<String> {
    let well_known = format!("https://{host}/.well-known/nodeinfo");
    let doc = fetch_json(core, &well_known).await?;
    let href = nodeinfo_doc_href(&doc)?;
    let doc2 = fetch_json(core, &href).await?;
    doc2.get("software")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .map(str::to_lowercase)
}

/// Pick the nodeinfo document link: prefer the 2.0 rel, else any
/// `…/ns/doc/…` href.
fn nodeinfo_doc_href(doc: &Value) -> Option<String> {
    let links = doc.get("links").and_then(Value::as_array)?;
    let mut fallback = None;
    for link in links {
        let Some(href) = link.get("href").and_then(Value::as_str) else {
            continue;
        };
        let rel = link.get("rel").and_then(Value::as_str).unwrap_or("");
        if rel == "http://nodeinfo.diaspora.software/ns/doc/2.0" {
            return Some(href.to_string());
        }
        if rel.contains("/ns/doc/") && fallback.is_none() {
            fallback = Some(href.to_string());
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_20_link_then_any_doc_link() {
        let doc = json!({
            "links": [
                {"rel": "http://nodeinfo.diaspora.software/ns/doc/1.0", "href": "https://h/ni/1"},
                {"rel": "http://nodeinfo.diaspora.software/ns/doc/2.0", "href": "https://h/ni/2"},
            ]
        });
        assert_eq!(nodeinfo_doc_href(&doc).as_deref(), Some("https://h/ni/2"));
        let doc = json!({
            "links": [
                {"rel": "self", "href": "https://h/x"},
                {"rel": "http://nodeinfo.diaspora.software/ns/doc/1.0", "href": "https://h/ni/1"},
            ]
        });
        assert_eq!(nodeinfo_doc_href(&doc).as_deref(), Some("https://h/ni/1"));
        assert!(nodeinfo_doc_href(&json!({"links": []})).is_none());
    }
}
