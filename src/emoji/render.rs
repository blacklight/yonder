//! Read-time `:shortcode:` substitution (epic 20).
//!
//! Stored `content_html` keeps the literal `:code:` text; the
//! substitution happens when an entity is assembled, so emoji added
//! or renamed later still render in old posts, bios and feeds. The
//! map is domain-scoped: remote statuses resolve against the author's
//! domain, never a same-named local emoji.
//!
//! The pass walks text runs only — `a`, `code`, `pre`, `script` and
//! `style` subtrees are skipped exactly like `posts::render`'s
//! enhancement pass, so code blocks and links are never touched.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::{config::Config, Result};

use super::{entity::emoji_url, model::EmojiRow};

/// One emoji in a [`EmojiMap`] — everything the `emojis` entity
/// array and the AP `Emoji` tag need.
#[derive(Debug)]
pub(crate) struct EmojiMeta {
    pub id: i64,
    /// The stored shortcode (original case; map keys are lowercase).
    pub shortcode: String,
    pub url: String,
    pub visible_in_picker: bool,
    pub category: Option<String>,
}

impl EmojiMeta {
    /// The AP `Emoji` tag entry for this emoji — emitted on objects
    /// (`tag`) so remote instances can render and record `:code:`.
    /// `base_url` mints the tag `id` (`{base}/emoji/{id}`).
    pub(crate) fn ap_tag(&self, base_url: &str) -> serde_json::Value {
        let mime = match self.url.rsplit('.').next() {
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("avif") => "image/avif",
            _ => "image/png",
        };
        serde_json::json!({
            "type": "Emoji",
            "id": format!("{}/emoji/{}", base_url.trim_end_matches('/'), self.id),
            "name": format!(":{}:", self.shortcode),
            "icon": { "type": "Image", "mediaType": mime, "url": self.url },
        })
    }
}

/// Shortcode (lowercased) → emoji metadata for one domain
/// (`None` = local).
pub struct EmojiMap {
    map: HashMap<String, EmojiMeta>,
}

impl EmojiMap {
    /// Empty map — substitution is a no-op.
    pub fn empty() -> Self {
        EmojiMap {
            map: HashMap::new(),
        }
    }

    /// Build from rows (already filtered by the caller).
    pub fn from_rows(rows: &[EmojiRow], config: &Config, base_url: &str) -> Self {
        let map = rows
            .iter()
            .filter_map(|r| {
                emoji_url(config, base_url, r).map(|url| {
                    (
                        r.shortcode.to_lowercase(),
                        EmojiMeta {
                            id: r.id,
                            shortcode: r.shortcode.clone(),
                            url,
                            visible_in_picker: r.visible_in_picker,
                            category: r.category.clone(),
                        },
                    )
                })
            })
            .collect();
        EmojiMap { map }
    }

    /// Enabled local emoji (`domain IS NULL`).
    pub async fn load_local(db: &PgPool, config: &Config, base_url: &str) -> Result<Self> {
        Ok(Self::from_rows(
            &super::model::local_enabled(db).await?,
            config,
            base_url,
        ))
    }

    /// Enabled emoji for one remote domain.
    pub async fn load_for_domain(
        db: &PgPool,
        config: &Config,
        base_url: &str,
        domain: &str,
    ) -> Result<Self> {
        Ok(Self::from_rows(
            &super::model::domain_enabled(db, domain).await?,
            config,
            base_url,
        ))
    }

    /// The map for one author: `None` → local, `Some(d)` → domain-scoped.
    pub async fn for_author(
        db: &PgPool,
        config: &Config,
        base_url: &str,
        domain: Option<&str>,
    ) -> Result<Self> {
        match domain {
            Some(d) => Self::load_for_domain(db, config, base_url, d).await,
            None => Self::load_local(db, config, base_url).await,
        }
    }

    /// [`for_author`] with a cheap short-circuit: when none of
    /// `texts` can hold a `:code:` (see [`has_shortcode_candidate`]),
    /// substitution is impossible — skip the database round-trip.
    pub async fn for_author_texts(
        db: &PgPool,
        config: &Config,
        base_url: &str,
        domain: Option<&str>,
        texts: &[&str],
    ) -> Result<Self> {
        if texts.iter().all(|t| !has_shortcode_candidate(t)) {
            return Ok(Self::empty());
        }
        Self::for_author(db, config, base_url, domain).await
    }

    /// The image URL for a shortcode (case-insensitive).
    pub fn get(&self, code: &str) -> Option<&str> {
        self.map.get(&code.to_lowercase()).map(|m| m.url.as_str())
    }

    /// Emoji referenced in `text` — scans `:code:` candidates once
    /// and looks each up (case-insensitive, matching `substitute`),
    /// so a `:BlobCat:` in the text yields the `blobcat` entry.
    /// Sorted by shortcode for deterministic output.
    pub(crate) fn used_in(&self, text: &str) -> Vec<&EmojiMeta> {
        if self.map.is_empty() || !text.contains(':') {
            return Vec::new();
        }
        let lower = text.to_lowercase();
        let bytes = lower.as_bytes();
        let mut seen: Vec<&EmojiMeta> = Vec::new();
        let mut pos = 0usize;
        while pos < lower.len() {
            if bytes[pos] == b':' {
                if let Some(close) = lower[pos + 1..].find(':') {
                    let code = &lower[pos + 1..pos + 1 + close];
                    if !code.is_empty()
                        && code.len() <= 100
                        && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        if let Some(meta) = self.map.get(code) {
                            if !seen.iter().any(|m: &&EmojiMeta| m.id == meta.id) {
                                seen.push(meta);
                            }
                        }
                        pos += close + 2;
                        continue;
                    }
                }
            }
            pos += 1;
        }
        seen.sort_by(|a, b| a.shortcode.cmp(&b.shortcode));
        seen
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `emojis` entity entries for every shortcode used in `text`.
    pub fn entities_in(&self, text: &str) -> Vec<serde_json::Value> {
        self.used_in(text)
            .into_iter()
            .map(|m| {
                serde_json::json!({
                    "shortcode": m.shortcode,
                    "url": m.url,
                    "static_url": m.url,
                    "visible_in_picker": m.visible_in_picker,
                    "category": m.category,
                })
            })
            .collect()
    }
}

/// Whether `text` contains something shaped like `:code:` — an
/// ASCII colon, a run of 1..=100 `[A-Za-z0-9_]` chars, then a closing
/// colon. Same candidate rule `substitute`/`used_in` apply minus the
/// map lookup; a bare `:` test would fire on every `https://` URL
/// and every piece of JSON, making the load short-circuit useless.
pub fn has_shortcode_candidate(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut pos = 0usize;
    while pos < text.len() {
        if bytes[pos] == b':' {
            if let Some(close) = text[pos + 1..].find(':') {
                let code = &text[pos + 1..pos + 1 + close];
                if !code.is_empty()
                    && code.len() <= 100
                    && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    return true;
                }
            }
        }
        pos += 1;
    }
    false
}

/// Elements whose subtrees are never tokenized — mirrors
/// `posts::render::SKIP_TAGS`.
const SKIP_TAGS: &[&str] = &["a", "code", "pre", "script", "style"];

/// Replace `:code:` inside `html`'s text runs with
/// `<img class="emoji">` — unknown shortcodes stay verbatim. The
/// substitution is domain-agnostic: the caller supplies the map.
pub fn substitute(html: &str, map: &EmojiMap) -> String {
    if map.is_empty() || !html.contains(':') {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + html.len() / 4);
    let mut skip_depth = 0usize;
    let mut pos = 0usize;
    let bytes = html.as_bytes();
    while pos < html.len() {
        if bytes[pos] == b'<' {
            if let Some(end) = html[pos..].find('>') {
                let tag = &html[pos..pos + end + 1];
                update_skip(tag, &mut skip_depth);
                out.push_str(tag);
                pos += end + 1;
                continue;
            }
        }
        let end = html[pos..].find('<').map(|e| pos + e).unwrap_or(html.len());
        let segment = &html[pos..end];
        if skip_depth == 0 {
            out.push_str(&substitute_segment(segment, map));
        } else {
            out.push_str(segment);
        }
        pos = end;
    }
    out
}

/// Track entry/exit into a [`SKIP_TAGS`] element — same shape as
/// `posts::render::update_skip`.
fn update_skip(tag: &str, depth: &mut usize) {
    let closing = tag.starts_with("</");
    let name: String = tag
        .trim_start_matches('<')
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if !SKIP_TAGS.contains(&name.as_str()) {
        return;
    }
    if closing {
        *depth = depth.saturating_sub(1);
    } else if !tag.ends_with("/>") {
        *depth += 1;
    }
}

/// Substitute inside one text segment.
fn substitute_segment(segment: &str, map: &EmojiMap) -> String {
    let mut out = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut pos = 0usize;
    while pos < segment.len() {
        if bytes[pos] == b':' {
            // Candidate `:code:` — alnum/underscore run then a colon.
            if let Some(close) = segment[pos + 1..].find(':') {
                let code = &segment[pos + 1..pos + 1 + close];
                if !code.is_empty()
                    && code.len() <= 100
                    && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    if let Some(url) = map.get(code) {
                        out.push_str(&format!(
                            "<img class=\"emoji\" alt=\":{code}:\" title=\":{code}:\" \
                             src=\"{}\" width=\"20\" height=\"20\">",
                            pubby_sdk::content::html_escape(url)
                        ));
                        pos += close + 2;
                        continue;
                    }
                }
            }
        }
        out.push(segment[pos..].chars().next().unwrap());
        pos += segment[pos..].chars().next().unwrap().len_utf8();
    }
    out
}

/// Apply a pre-built map to an assembled Account entity: `note` and
/// `fields[].value` get `<img>` substitution (both are already-
/// rendered HTML); `display_name` keeps its raw text — Mastodon
/// serves it as plain text and clients render `:code:` themselves
/// from `emojis`, which lists every shortcode used across all three.
pub fn apply_account_with_map(entity: &mut serde_json::Value, map: &EmojiMap) {
    let mut haystack = String::new();
    if let Some(note) = entity
        .get("note")
        .and_then(|n| n.as_str())
        .map(str::to_string)
    {
        haystack.push_str(&note);
        entity["note"] = serde_json::Value::String(substitute(&note, map));
    }
    if let Some(fields) = entity.get_mut("fields").and_then(|f| f.as_array_mut()) {
        for f in fields.iter_mut() {
            if let Some(v) = f.get("value").and_then(|v| v.as_str()).map(str::to_string) {
                haystack.push_str(&v);
                f["value"] = serde_json::Value::String(substitute(&v, map));
            }
        }
    }
    // Haystack only — `display_name` stays plain text (see doc).
    if let Some(name) = entity.get("display_name").and_then(|n| n.as_str()) {
        haystack.push_str(name);
    }
    entity["emojis"] = serde_json::Value::Array(map.entities_in(&haystack));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> EmojiMap {
        let mut m = HashMap::new();
        m.insert(
            "blobcat".to_string(),
            EmojiMeta {
                id: 1,
                shortcode: "blobcat".to_string(),
                url: "https://y.test/media/emoji/1/x.png".to_string(),
                visible_in_picker: true,
                category: None,
            },
        );
        EmojiMap { map: m }
    }

    #[test]
    fn substitutes_known_shortcode() {
        let html = substitute("hi :blobcat: bye", &map());
        assert!(html.contains("<img class=\"emoji\""));
        assert!(html.contains("alt=\":blobcat:\""));
        assert!(!html.contains(":blobcat: bye"));
    }

    #[test]
    fn unknown_stays_verbatim() {
        assert_eq!(substitute("hi :nope: bye", &map()), "hi :nope: bye");
        assert_eq!(substitute("::double::", &map()), "::double::");
    }

    #[test]
    fn skips_code_and_links() {
        let html = substitute(
            "<code>:blobcat:</code> text :blobcat: <a href=\"x\">:blobcat:</a>",
            &map(),
        );
        assert_eq!(html.matches("<img class=\"emoji\"").count(), 1);
        assert!(html.contains("<code>:blobcat:</code>"));
        assert!(html.contains("<a href=\"x\">:blobcat:</a>"));
    }

    #[test]
    fn escapes_url_in_attribute() {
        let mut m = HashMap::new();
        m.insert(
            "x1".to_string(),
            EmojiMeta {
                id: 2,
                shortcode: "x1".to_string(),
                url: "https://e/?a=1&b=2".to_string(),
                visible_in_picker: true,
                category: None,
            },
        );
        let map = EmojiMap { map: m };
        let html = substitute(":x1:", &map);
        assert!(html.contains("a=1&amp;b=2"));
        assert!(!html.contains("a=1&b=2\""));
    }

    #[test]
    fn used_in_is_case_insensitive() {
        let m = map();
        let used = m.used_in("hello :BlobCat: world");
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].shortcode, "blobcat");
        assert!(m.used_in("nothing here").is_empty());
    }

    #[test]
    fn display_name_stays_text_but_feeds_emojis() {
        let mut entity = serde_json::json!({
            "note": "bio :blobcat:",
            "display_name": "Name :blobcat: <b>",
            "fields": [],
        });
        apply_account_with_map(&mut entity, &map());
        assert!(entity["note"].as_str().unwrap().contains("<img"));
        // display_name is plain text — no markup, not escaped.
        assert_eq!(entity["display_name"], "Name :blobcat: <b>");
        // …but its shortcode still lands in `emojis` once (dedup).
        assert_eq!(entity["emojis"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn case_insensitive_lookup() {
        assert!(substitute(":BlobCat:", &map()).contains("<img"));
    }

    #[test]
    fn multibyte_around_shortcode() {
        let html = substitute("héllo :blobcat: 👋", &map());
        assert!(html.contains("héllo"));
        assert!(html.contains("👋"));
    }
}
