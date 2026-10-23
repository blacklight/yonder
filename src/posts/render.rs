//! Status HTML rendering — epic 12's hardened pipeline.
//!
//! Two source formats, one output contract: sanitized HTML safe to
//! serve in the Status entity and the AP `content` field.
//!
//! - `text/plain` renders through pubby's [`render_post_html`] (full
//!   escaping, http(s) linkification, `#tag` → `rel="tag"` anchors,
//!   `<br>` newlines); resolved `@mentions` are then turned into
//!   profile anchors by [`enhance_html`].
//! - `text/markdown` renders through `pulldown-cmark` and then the
//!   [`ammonia`] allowlist — no `script`/`style`/`iframe`/`img`
//!   (remote images are a tracking/SSRF hazard), `href` schemes
//!   restricted to `content.valid_url_schemes`. The same
//!   [`enhance_html`] pass linkifies bare URLs/`#tags`/`@mentions` in
//!   text nodes only (`code`/`pre`/`a` subtrees are skipped, so code
//!   spans and already-rendered links are never touched) and collects
//!   the hashtag list.
//!
//! [`sanitize_html`] reuses the same allowlist for inbound remote
//! `content` — remote HTML is never trusted raw.

use std::collections::HashSet;

use pubby_sdk::{
    content::{html_escape, RenderedContent},
    is_linkable_url, render_link_anchor, render_post_html,
};

use crate::config::Config;

/// What a render pass produced: the sanitized HTML plus the normalized
/// hashtag names found (feed `tags`/`status_tags`).
#[derive(Debug, Clone)]
pub struct RenderedPost {
    /// Sanitized HTML — stored in `statuses.content_html`.
    pub html: String,
    /// Normalized (lowercase) hashtag names, in encounter order.
    pub hashtags: Vec<String>,
}

/// A resolved mention for the enhancement pass: the literal `@token`
/// as it appears in source (e.g. `@alice`, `@bob@remote.example`) and
/// the profile/actor URL to link to.
#[derive(Debug, Clone)]
pub struct MentionLink {
    /// The literal mention token in the text, without ambiguity.
    pub token: String,
    /// Absolute URL the anchor points to.
    pub href: String,
}

/// The `#tag` target URL: `{base}/tags/{name}` — Mastodon's shape.
pub fn hashtag_url(base_url: &str) -> impl Fn(&str) -> String + '_ {
    move |name: &str| format!("{}/tags/{}", base_url.trim_end_matches('/'), name)
}

/// Render `source` per `content_type`. `mentions` are the resolved
/// tokens from [`crate::posts::mentions`] (unresolved tokens stay
/// escaped text). Falls back to plain rendering for unknown content
/// types — callers validate the type beforehand.
pub fn render(
    source: &str,
    content_type: &str,
    mentions: &[MentionLink],
    base_url: &str,
    config: &Config,
) -> RenderedPost {
    let tag_url = hashtag_url(base_url);
    match content_type {
        "text/markdown" => {
            let mut html = String::new();
            let options = pulldown_cmark::Options::ENABLE_TABLES
                | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
                | pulldown_cmark::Options::ENABLE_TASKLISTS;
            let parser = pulldown_cmark::Parser::new_ext(source, options);
            pulldown_cmark::html::push_html(&mut html, parser);
            let html = sanitize_html(&html, config);
            let (html, hashtags) = enhance_html(&html, mentions, &tag_url, config);
            RenderedPost { html, hashtags }
        }
        _ => {
            let RenderedContent { html, mut hashtags } = render_post_html(source, &tag_url);
            let (html, found) = enhance_html(&html, mentions, &tag_url, config);
            for tag in found {
                if !hashtags.contains(&tag) {
                    hashtags.push(tag);
                }
            }
            RenderedPost { html, hashtags }
        }
    }
}

/// Plain text of an HTML fragment, whitespace-collapsed and capped at
/// 20000 chars — the searchable `content` of a remote status that
/// carries no `source` text.
pub fn html_to_text(html: &str) -> String {
    scraper::Html::parse_fragment(html)
        .root_element()
        .text()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(20000)
        .collect()
}

/// The hardened allowlist applied to markdown output and inbound
/// remote HTML. `a[href]` is limited to `content.valid_url_schemes`;
/// every other attribute is stripped.
pub fn sanitize_html(html: &str, config: &Config) -> String {
    let tags: HashSet<&'static str> = [
        "p",
        "br",
        "a",
        "em",
        "strong",
        "b",
        "i",
        "u",
        "s",
        "del",
        "code",
        "pre",
        "blockquote",
        "ul",
        "ol",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
        "span",
        "sub",
        "sup",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
    ]
    .into_iter()
    .collect();
    let schemes: HashSet<&str> = config
        .content
        .valid_url_schemes
        .iter()
        .map(String::as_str)
        .collect();
    let mut a_attrs: HashSet<&'static str> = ["href"].into_iter().collect();
    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .link_rel(Some("nofollow noopener"))
        .url_schemes(schemes);
    let mut tag_attrs = std::collections::HashMap::new();
    tag_attrs.insert("a", std::mem::take(&mut a_attrs));
    builder.tag_attributes(tag_attrs);
    builder.clean(html).to_string()
}

/// The CW/summary render: escaped plain text, never markup.
pub fn render_spoiler(text: &str) -> String {
    html_escape(text)
}

// ---------------------------------------------------------------------------
// Text-node enhancement
// ---------------------------------------------------------------------------

/// Elements whose subtrees are never tokenized: existing anchors and
/// code/preformatted content stay verbatim.
const SKIP_TAGS: &[&str] = &["a", "code", "pre", "script", "style"];

/// Linkify bare URLs, `#hashtags` and resolved `@mentions` inside the
/// *text* of an HTML document, skipping [`SKIP_TAGS`] subtrees.
/// `html` must already be sanitized — text outside tags passes through
/// untouched, tags are copied verbatim.
///
/// Returns `(html, hashtags)` — the normalized hashtag names linked.
pub fn enhance_html(
    html: &str,
    mentions: &[MentionLink],
    hashtag_url: &dyn Fn(&str) -> String,
    config: &Config,
) -> (String, Vec<String>) {
    let mut out = String::with_capacity(html.len() + html.len() / 4);
    let mut hashtags: Vec<String> = Vec::new();
    let mut skip_depth: usize = 0;
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
        // Text run up to the next tag.
        let end = html[pos..].find('<').map(|e| pos + e).unwrap_or(html.len());
        let segment = &html[pos..end];
        if skip_depth == 0 {
            let (rendered, found) = enhance_segment(segment, mentions, hashtag_url, config);
            out.push_str(&rendered);
            for tag in found {
                if !hashtags.contains(&tag) {
                    hashtags.push(tag);
                }
            }
        } else {
            out.push_str(segment);
        }
        pos = end;
    }
    (out, hashtags)
}

/// Track entry into / exit from a [`SKIP_TAGS`] element. Tag text like
/// `<a href=…>` or `</code>` — attributes never reach here malformed
/// since the input is sanitized.
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

/// Whether the character before a `#`/`@` candidate disqualifies it
/// (same rule as pubby's `hashtag_start_ok`): word chars, `_`, `&`
/// (entity), `#` (nested).
fn boundary_ok(text: &str, start: usize) -> bool {
    match text[..start].chars().last() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_' || c == '&' || c == '#'),
    }
}

/// Split trailing punctuation/entities off a URL token — the same
/// rule pubby applies (sentence-final `.` `,` `)` … and HTML entities
/// that closed the sentence are not part of the link). `pub(crate)`
/// for the link-preview extractor.
pub(crate) fn split_url_trailing(token: &str) -> (&str, &str) {
    let mut end = token.len();
    loop {
        let s = &token[..end];
        if s.ends_with("&amp;") || s.ends_with("&quot;") || s.ends_with("&#39;") {
            end = s.rfind('&').unwrap_or(end);
            continue;
        }
        match s.chars().last() {
            Some(last)
                if matches!(
                    last,
                    '.' | ',' | '!' | '?' | ':' | ';' | '\'' | '"' | ')' | ']' | '}'
                ) =>
            {
                end -= last.len_utf8();
            }
            _ => break,
        }
    }
    (&token[..end], &token[end..])
}

/// Whether `url`'s scheme is allowed by `content.valid_url_schemes`.
/// `pub(crate)` for the link-preview extractor.
pub(crate) fn scheme_allowed(url: &str, config: &Config) -> bool {
    match url.split_once("://") {
        Some((scheme, _)) => config
            .content
            .valid_url_schemes
            .iter()
            .any(|s| s.eq_ignore_ascii_case(scheme)),
        None => false,
    }
}

/// Tokenize one text segment and emit enhanced HTML. URLs become
/// anchors (scheme-less label via pubby's `render_link_anchor` default
/// label), `#tags` get `rel="tag"` and resolved `@mentions` get
/// profile anchors; everything else passes through verbatim.
fn enhance_segment(
    segment: &str,
    mentions: &[MentionLink],
    hashtag_url: &dyn Fn(&str) -> String,
    config: &Config,
) -> (String, Vec<String>) {
    let mut out = String::with_capacity(segment.len() + segment.len() / 4);
    let mut hashtags: Vec<String> = Vec::new();
    let bytes = segment.as_bytes();
    let mut pos = 0usize;

    while pos < segment.len() {
        let rest = &segment[pos..];
        let c = bytes[pos];
        // URL token — `http(s)://` or another allowed scheme.
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|ch: char| {
                    ch.is_whitespace() || ch == '<' || ch == '>' || ch == '"' || ch == '\''
                })
                .map(|e| pos + e)
                .unwrap_or(segment.len());
            let raw = &segment[pos..end];
            let (url, tail) = split_url_trailing(raw);
            if url.len() > "https://".len() && scheme_allowed(url, config) && is_linkable_url(url) {
                out.push_str(&render_link_anchor(url, None, None));
            } else {
                out.push_str(url);
            }
            // Anything stripped as trailing punctuation is plain text.
            out.push_str(tail);
            pos = end;
            continue;
        }
        if c == b'#' {
            let name_end = rest[1..]
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .map(|e| pos + 1 + e)
                .unwrap_or(segment.len());
            let name = &segment[pos + 1..name_end];
            if !name.is_empty()
                && name.chars().any(|ch| ch.is_alphabetic())
                && boundary_ok(segment, pos)
            {
                let normalized = name.to_lowercase();
                out.push_str(&render_link_anchor(
                    &hashtag_url(&normalized),
                    Some("tag"),
                    Some(&segment[pos..name_end]),
                ));
                if !hashtags.contains(&normalized) {
                    hashtags.push(normalized);
                }
            } else {
                out.push_str(&segment[pos..name_end]);
            }
            pos = name_end;
            continue;
        }
        if c == b'@' {
            if let Some(m) = mentions
                .iter()
                .find(|m| rest.starts_with(&m.token) && boundary_ok(segment, pos))
            {
                let end = pos + m.token.len();
                // The token must terminate — a longer handle would mean
                // a different (unresolved) mention.
                let terminates = match segment[end..].chars().next() {
                    None => true,
                    Some(ch) => !(ch.is_alphanumeric() || ch == '_' || ch == '@'),
                };
                if terminates {
                    out.push_str(&render_link_anchor(&m.href, None, Some(&m.token)));
                    pos = end;
                    continue;
                }
            }
            out.push('@');
            pos += 1;
            continue;
        }
        out.push(segment[pos..].chars().next().unwrap());
        pos += segment[pos..].chars().next().unwrap().len_utf8();
    }
    (out, hashtags)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn plain_links_hashtags_newlines() {
        let r = render(
            "hi https://example.com/x #Rust\nnext",
            "text/plain",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains("href=\"https://example.com/x\""));
        assert!(r.html.contains("rel=\"tag\""));
        assert!(r.html.contains("#Rust"));
        assert!(r.html.contains("<br>"));
        assert_eq!(r.hashtags, vec!["rust"]);
    }

    #[test]
    fn mention_anchor_plain() {
        let mentions = vec![MentionLink {
            token: "@alice".to_string(),
            href: "https://y.test/@alice".to_string(),
        }];
        let r = render(
            "hey @alice, look",
            "text/plain",
            &mentions,
            "https://y.test",
            &cfg(),
        );
        assert!(r
            .html
            .contains("<a href=\"https://y.test/@alice\">@alice</a>"));
    }

    #[test]
    fn mention_inside_url_not_double_linked() {
        let mentions = vec![MentionLink {
            token: "@alice".to_string(),
            href: "https://y.test/@alice".to_string(),
        }];
        let r = render(
            "see https://ex.com/@alice here",
            "text/plain",
            &mentions,
            "https://y.test",
            &cfg(),
        );
        // One anchor for the URL only; no mention anchor inside it.
        assert_eq!(r.html.matches("href=").count(), 1);
    }

    #[test]
    fn markdown_renders_and_sanitizes() {
        let r = render(
            "# Title\n\n**bold** <script>alert(1)</script> [x](javascript:alert(1))",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains("<h1>"));
        assert!(r.html.contains("<strong>bold</strong>"));
        assert!(!r.html.contains("<script>"));
        assert!(!r.html.contains("javascript:"));
    }

    #[test]
    fn markdown_code_untouched() {
        let r = render(
            "text `#not_a_tag http://not.a/link` end",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains("<code>"));
        assert_eq!(r.hashtags.len(), 0);
        assert!(!r.html.contains("not.a/link\""));
    }

    #[test]
    fn markdown_enhances_bare_url_and_tag() {
        let r = render(
            "para https://example.com #tag",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains("href=\"https://example.com\""));
        assert!(r.html.contains("rel=\"tag\""));
        assert_eq!(r.hashtags, vec!["tag"]);
    }

    #[test]
    fn cw_is_escaped() {
        assert_eq!(render_spoiler("<b>x</b>"), "&lt;b&gt;x&lt;/b&gt;");
    }
}
