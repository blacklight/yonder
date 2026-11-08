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
//!   [`ammonia`] allowlist — no `script`/`style`/`iframe`, `href`/`src`
//!   schemes restricted to `content.valid_url_schemes`. `![alt](url)`
//!   images are the one deliberate exception to the no-`img` rule:
//!   they render inline with the alt text repeated as a visible
//!   caption, and [`markdown_images`] exposes the same URLs/alt text so
//!   ActivityPub can also deliver them as standard attachments for
//!   implementations without inline-image support. The same
//!   [`enhance_html`] pass linkifies bare URLs/`#tags`/`@mentions` in
//!   text nodes only (`code`/`pre`/`a`/caption subtrees are skipped, so
//!   code spans, already-rendered links and captions are never
//!   touched) and collects the hashtag list.
//!
//! [`sanitize_html`] reuses the strict allowlist for inbound remote
//! `content` — remote HTML is never trusted raw.

use std::collections::HashSet;
use std::sync::LazyLock;

use pubby_sdk::{
    content::{html_escape, RenderedContent},
    is_linkable_url, render_link_anchor, render_post_html,
};
use regex::Regex;

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
    /// The resolved acct without a leading `@` — `alice` for a local
    /// account, `alice@remote.example` for a remote one. Drives the
    /// anchor label (see [`mention_label`]).
    pub acct: String,
}

/// The `#tag` target URL: `{base}/tags/{name}` — Mastodon's shape,
/// with the name percent-encoded as one path segment (`#Scheinkünstler`
/// → `/tags/scheink%C3%BCnstler`).
pub fn hashtag_url(base_url: &str) -> impl Fn(&str) -> String + '_ {
    let base = base_url.trim_end_matches('/');
    move |name: &str| crate::feeds::data::tag_url(base, name)
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
            // Lift math out of the source before pulldown-cmark can
            // eat backslash escapes (`\\` → `\`, `\,` → `,`) — the
            // rendered HTML then carries placeholder tokens swapped
            // for `data-yonder-math` spans below.
            let (masked, math_spans) = if config.math.enabled {
                let protected = markdown_math_protected(source, options);
                super::math::extract_source_math(source, &protected, &config.math)
            } else {
                (source.to_string(), Vec::new())
            };
            let parser = pulldown_cmark::Parser::new_ext(&masked, options);
            pulldown_cmark::html::push_html(&mut html, parser);
            let html = sanitize_markdown_html(&html, config);
            let html = inline_image_spans(&html);
            let (html, hashtags) = enhance_html(&html, mentions, &tag_url, config);
            let html = super::math::materialize_tokens(&html, &math_spans);
            let html = super::math::inject_markers(&html, &config.math);
            RenderedPost { html, hashtags }
        }
        _ => {
            let (masked, math_spans) = if config.math.enabled {
                let protected = plain_url_ranges(source);
                super::math::extract_source_math(source, &protected, &config.math)
            } else {
                (source.to_string(), Vec::new())
            };
            let RenderedContent { html, mut hashtags } = render_post_html(&masked, &tag_url);
            let (html, found) = enhance_html(&html, mentions, &tag_url, config);
            for tag in found {
                if !hashtags.contains(&tag) {
                    hashtags.push(tag);
                }
            }
            let html = super::math::materialize_tokens(&html, &math_spans);
            let html = super::math::inject_markers(&html, &config.math);
            RenderedPost { html, hashtags }
        }
    }
}

/// Source byte ranges in Markdown `source` where math must stay
/// literal — inline code, code blocks, raw HTML, footnote refs, and
/// everything inside links/images (matching the `code`/`a` skip list
/// [`super::math::inject_markers`] applies to rendered HTML). Link and
/// image containers are tracked by depth so their text *and*
/// destination stay protected regardless of how pulldown spans the
/// tag events.
fn markdown_math_protected(
    source: &str,
    options: pulldown_cmark::Options,
) -> Vec<std::ops::Range<usize>> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let mut ranges = Vec::new();
    let mut link_depth = 0usize;
    for (event, range) in pulldown_cmark::Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                link_depth += 1;
                ranges.push(range);
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                link_depth = link_depth.saturating_sub(1);
                ranges.push(range);
            }
            _ if link_depth > 0 => ranges.push(range),
            Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_) => ranges.push(range),
            _ => {}
        }
    }
    ranges.sort_by_key(|r| r.start);
    ranges
}

/// Source byte ranges of http(s) URLs in plain text — the same pattern
/// pubby's [`render_post_html`] linkifies, so `$…$` inside a URL is
/// never mistaken for math (it would render inside the anchor anyway).
fn plain_url_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    static URL_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"https?://[^\s<>"']+"#).unwrap());
    URL_RE.find_iter(source).map(|m| m.range()).collect()
}

/// A Markdown `![alt](url)` image, normalized for federation: the URL
/// is scheme-checked against `content.valid_url_schemes` and the alt
/// text is the flattened image description (capped like media alt
/// text). Deduplicated by `(url, alt)` in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    /// The image URL as written in the Markdown source.
    pub url: String,
    /// Flattened alt text (empty when the image has none).
    pub alt: String,
}

/// Extract the renderable inline images from Markdown `source`.
/// `pulldown-cmark` only reports image syntax — code spans/blocks and
/// raw HTML never produce events — and the same scheme allowlist the
/// HTML sanitizer applies keeps `javascript:`/`data:` URLs out of the
/// ActivityPub `attachment` list.
pub fn markdown_images(source: &str, config: &Config) -> Vec<InlineImage> {
    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS;
    let mut events = pulldown_cmark::Parser::new_ext(source, options);
    let mut images = Vec::new();
    let mut seen = HashSet::new();

    while let Some(event) = events.next() {
        let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Image { dest_url, .. }) = event
        else {
            continue;
        };
        let mut alt = String::new();
        let mut depth = 0usize;
        for event in events.by_ref() {
            match event {
                pulldown_cmark::Event::Start(_) => depth += 1,
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Image) if depth == 0 => break,
                pulldown_cmark::Event::End(_) => depth = depth.saturating_sub(1),
                pulldown_cmark::Event::Text(text) | pulldown_cmark::Event::Code(text) => {
                    alt.push_str(&text);
                }
                pulldown_cmark::Event::InlineMath(text)
                | pulldown_cmark::Event::DisplayMath(text) => {
                    alt.push('$');
                    alt.push_str(&text);
                    alt.push('$');
                }
                pulldown_cmark::Event::SoftBreak | pulldown_cmark::Event::HardBreak => {
                    alt.push(' ');
                }
                _ => {}
            }
        }
        let alt: String = alt.split_whitespace().collect::<Vec<_>>().join(" ");
        let alt: String = alt.chars().take(5000).collect();
        if !scheme_allowed(&dest_url, config) || !seen.insert((dest_url.clone(), alt.clone())) {
            continue;
        }
        images.push(InlineImage {
            url: dest_url.to_string(),
            alt,
        });
    }
    images
}

/// Plain text of an HTML fragment, whitespace-collapsed and capped at
/// 20000 chars — the searchable `content` of a remote status that
/// carries no `source` text.
///
/// Anchors are tokens, not containers. A link whose visible text is
/// itself a URL — Mastodon renders links as `href` +
/// `<span class="invisible">scheme</span>` + truncated/ellipsis spans,
/// which flattens to a broken `https:// host/…` — emits its `href`:
/// the href is the whole URL, so search and the link-preview
/// extractor see the real target. `@`/`#` single-token anchors
/// (mentions, hashtags) emit their text compacted (`@ user` →
/// `@user`). Descriptive anchors keep their text — a link the author
/// didn't spell out never becomes a preview candidate.
pub fn html_to_text(html: &str) -> String {
    use scraper::node::Node;
    fn walk(el: &scraper::ElementRef<'_>, out: &mut Vec<String>) {
        for child in el.children() {
            match child.value() {
                Node::Text(text) => out.extend(text.split_whitespace().map(str::to_string)),
                Node::Element(elem) => {
                    let Some(eref) = scraper::ElementRef::wrap(child) else {
                        continue;
                    };
                    if elem.name() == "a" {
                        let text: String = eref.text().collect();
                        let trimmed = text.trim();
                        let compact: String =
                            trimmed.chars().filter(|c| !c.is_whitespace()).collect();
                        if compact.starts_with("http://") || compact.starts_with("https://") {
                            if let Some(href) = elem
                                .attr("href")
                                .filter(|h| h.starts_with("http://") || h.starts_with("https://"))
                            {
                                out.push(href.to_string());
                                continue;
                            }
                        }
                        if let Some(rest) = trimmed.strip_prefix(['@', '#']) {
                            let rest = rest.trim();
                            if !rest.is_empty() && !rest.contains(char::is_whitespace) {
                                out.push(compact);
                                continue;
                            }
                        }
                    }
                    // A math placeholder flattens back to its
                    // delimited source so `content` keeps `$…$`.
                    if elem.name() == "span" {
                        if let Some(mode) = elem.attr("data-yonder-math") {
                            let inner: String = eref.text().collect();
                            let d = if mode == "display" { "$$" } else { "$" };
                            out.push(format!("{d}{inner}{d}"));
                            continue;
                        }
                    }
                    walk(&eref, out);
                }
                _ => {}
            }
        }
    }
    let fragment = scraper::Html::parse_fragment(html);
    let mut out = Vec::new();
    walk(&fragment.root_element(), &mut out);
    out.join(" ").chars().take(20000).collect()
}

/// The hardened allowlist applied to inbound remote HTML. `a[href]`
/// is limited to `content.valid_url_schemes`; every other attribute is
/// stripped.
pub fn sanitize_html(html: &str, config: &Config) -> String {
    sanitize_html_inner(html, config, false)
}

/// Markdown output gets the same allowlist plus `img[src,alt]` — the
/// only way `![alt](url)` can render. The scheme allowlist still
/// applies to `src`, so `javascript:`/`data:` images never survive.
fn sanitize_markdown_html(html: &str, config: &Config) -> String {
    sanitize_html_inner(html, config, true)
}

/// Remote HTML ingest: same allowlist with `img` permitted so the
/// caller can keep attachment-backed/long-form inline images and drop
/// everything else ([`inline_remote_images`]).
pub(crate) fn sanitize_html_allowing_images(html: &str, config: &Config) -> String {
    sanitize_html_inner(html, config, true)
}

fn sanitize_html_inner(html: &str, config: &Config, allow_images: bool) -> String {
    let mut tags: HashSet<&'static str> = [
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
    if allow_images {
        tags.insert("img");
    }
    let schemes: HashSet<&str> = config
        .content
        .valid_url_schemes
        .iter()
        .map(String::as_str)
        .collect();
    // `data-yonder-math-image`/`data-yonder-math-alt` mark server-rendered
    // formula markup on inbound posts — without them a Yonder→Yonder
    // round-trip loses the inline/display styling and the LaTeX-source
    // fallback link.
    let a_attrs: HashSet<&'static str> = ["href", "data-yonder-math-alt"].into_iter().collect();
    let span_attrs: HashSet<&'static str> = ["data-yonder-math-image"].into_iter().collect();
    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .link_rel(Some("nofollow noopener"))
        .url_schemes(schemes);
    // `class` survives only on `code`/`pre` — pulldown-cmark emits
    // `language-*` on fenced blocks and clients use it for syntax
    // highlighting. Everywhere else class is still stripped (mention/
    // hashtag markers must not reach the client).
    let code_attrs: HashSet<&'static str> = ["class"].into_iter().collect();
    let img_attrs: HashSet<&'static str> = ["src", "alt"].into_iter().collect();
    let mut tag_attrs = std::collections::HashMap::new();
    tag_attrs.insert("a", a_attrs);
    tag_attrs.insert("span", span_attrs);
    tag_attrs.insert("code", code_attrs.clone());
    tag_attrs.insert("pre", code_attrs);
    if allow_images {
        tag_attrs.insert("img", img_attrs);
    }
    builder.tag_attributes(tag_attrs);
    builder.clean(html).to_string()
}

/// The CW/summary render: escaped plain text, never markup.
pub fn render_spoiler(text: &str) -> String {
    html_escape(text)
}

// ---------------------------------------------------------------------------
// Inline images
// ---------------------------------------------------------------------------

static IMG_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<img\b[^>]*>"#).unwrap());
static IMG_ALT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\balt="([^"]*)""#).unwrap());
static IMG_SRC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bsrc="([^"]*)""#).unwrap());

/// Wrap every sanitized `<img>` in a `data-yonder-inline-image` span and
/// repeat its alt text as a visible caption. The span stays phrasing
/// content (valid inside `<p>` and `<a>`) while the frontend styles it
/// as a block; `data-*` markers survive sanitizing because the markup
/// is generated after the allowlist runs.
fn inline_image_spans(html: &str) -> String {
    inline_image_spans_where(html, |_| true).0
}

/// [`inline_image_spans`] generalized: `keep(src)` decides whether an
/// `<img>` becomes an inline image (kept `src`s are returned for the
/// caller's attachment bookkeeping) or is dropped entirely.
fn inline_image_spans_where(html: &str, keep: impl Fn(&str) -> bool) -> (String, Vec<String>) {
    if !html.contains("<img") {
        return (html.to_string(), Vec::new());
    }
    let mut kept = Vec::new();
    let out = IMG_TAG_RE
        .replace_all(html, |caps: &regex::Captures| {
            let tag = caps.get(0).unwrap().as_str();
            let Some(src) = IMG_SRC_RE
                .captures(tag)
                .and_then(|c| c.get(1))
                .map(|m| decode_html_entities(m.as_str()))
            else {
                return String::new();
            };
            if !keep(&src) {
                return String::new();
            }
            let alt = IMG_ALT_RE
                .captures(tag)
                .and_then(|c| c.get(1))
                .map(|m| decode_html_entities(m.as_str()))
                .unwrap_or_default();
            let alt = alt.trim();
            // Mastodon-style: the caption is a link to the image, so
            // the alt text doubles as the "open full image" affordance
            // when the image itself can't be shown.
            let wrapped = if alt.is_empty() {
                format!("<span data-yonder-inline-image>{tag}</span>")
            } else {
                format!(
                    "<span data-yonder-inline-image>{tag}<small data-yonder-inline-image-alt><a href=\"{}\">{}</a></small></span>",
                    html_escape(&src),
                    html_escape(alt)
                )
            };
            kept.push(src);
            wrapped
        })
        .to_string();
    (out, kept)
}

/// Remote-HTML inline-image pass. `allowed_srcs` are normalized
/// attachment URLs; `allow_all` keeps every sanitized `<img>` (used for
/// converted long-form objects whose images are content, not
/// attachments). Returns the rewritten HTML plus the normalized `src`s
/// that were kept inline — the caller uses them to skip duplicate
/// `media_attachments` rows.
pub(crate) fn inline_remote_images(
    html: &str,
    allowed_srcs: &HashSet<String>,
    allow_all: bool,
) -> (String, HashSet<String>) {
    let (html, kept) = inline_image_spans_where(html, |src| {
        allow_all || normalize_url_for_match(src).is_some_and(|u| allowed_srcs.contains(&u))
    });
    let kept: HashSet<String> = kept
        .into_iter()
        .filter_map(|s| normalize_url_for_match(&s))
        .collect();
    (html, kept)
}

/// Remote `content` HTML ingest pipeline: undo our own inline-image
/// wrapper markup back to bare `<img>`s (so captions aren't
/// duplicated), sanitize with `img` permitted, then keep only the
/// inline images [`inline_remote_images`] allows. Returns the HTML
/// plus the normalized `src`s kept inline.
pub(crate) fn render_remote_html(
    html: &str,
    config: &Config,
    allowed_srcs: &HashSet<String>,
    allow_all: bool,
) -> (String, HashSet<String>) {
    let html = strip_inline_image_markup(html);
    let html = sanitize_html_allowing_images(&html, config);
    inline_remote_images(&html, allowed_srcs, allow_all)
}

static INLINE_IMAGE_SPAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<span data-yonder-inline-image>\s*(<img\b[^>]*>)(?:<small data-yonder-inline-image-alt>.*?</small>)?\s*</span>"#,
    )
    .unwrap()
});

/// Collapse `<span data-yonder-inline-image><img>…</span>` markup (as
/// emitted by [`inline_image_spans`]) back to the bare `<img>` so a
/// remote round-trip doesn't double-wrap or duplicate captions.
fn strip_inline_image_markup(html: &str) -> String {
    if !html.contains("data-yonder-inline-image") {
        return html.to_string();
    }
    INLINE_IMAGE_SPAN_RE.replace_all(html, "$1").to_string()
}

/// Normalized `src`s of every `<img>` in already-rendered inline-image
/// markup — used to deduplicate `media_attachments` rows against
/// images that stayed in the post body.
pub(crate) fn inline_image_srcs(html: &str) -> HashSet<String> {
    IMG_TAG_RE
        .captures_iter(html)
        .filter_map(|c| IMG_SRC_RE.captures(c.get(0)?.as_str()))
        .filter_map(|c| c.get(1))
        .map(|m| decode_html_entities(m.as_str()))
        .filter_map(|s| normalize_url_for_match(&s))
        .collect()
}

/// Normalize a URL for inline-vs-attachment comparison: absolute
/// http(s) only, parsed through `url::Url` so trivial differences
/// (default ports, percent-encoding case, trailing `/` on the path)
/// still match. `None` for relative/non-http(s) values.
pub(crate) fn normalize_url_for_match(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let strip_trailing = url.path().len() > 1;
    let mut normalized: String = url.into();
    if strip_trailing && normalized.ends_with('/') {
        normalized.pop();
    }
    Some(normalized)
}

/// Decode the small entity set `html_escape`/pulldown-cmark emit, in a
/// single pass so `&amp;lt;` stays literal `&lt;` rather than `<`.
pub(crate) fn decode_html_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(end) = after.find(';') else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        if end > 10 {
            out.push('&');
            rest = &after[1..];
            continue;
        }
        let entity = &after[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&after[..=end]);
                rest = &after[end + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Text-node enhancement
// ---------------------------------------------------------------------------

/// Elements whose subtrees are never tokenized: existing anchors and
/// code/preformatted content stay verbatim.
const SKIP_TAGS: &[&str] = &["a", "code", "pre", "script", "style", "small"];

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
/// (entity), `#`/`＃` (nested).
fn boundary_ok(text: &str, start: usize) -> bool {
    match text[..start].chars().last() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_' || c == '&' || c == '#' || c == '＃'),
    }
}

/// A `#hashtag` token at the start of a text segment — Mastodon's
/// grammar (`Tag::HASHTAG_NAME_PAT` in `tag.rb`, the same pattern
/// pubby's `HASHTAG_RE` uses): Unicode word chars — letters, combining
/// marks, numbers, connector punctuation — with `·`, `・`, ZWNJ/ZWJ as
/// mid-tag separators and the fullwidth `＃` accepted as the marker.
/// Names start and end on a word char, so separators can't lead or
/// trail. Anchored: candidates further into the segment are found by
/// the scanner's own advance.
static HASHTAG_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^[#＃][\p{L}\p{M}\p{N}\p{Pc}]",
        r"(?:[\p{L}\p{M}\p{N}\p{Pc}·・\u{200C}\u{200D}]*[\p{L}\p{M}\p{N}\p{Pc}])?"
    ))
    .unwrap()
});

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

/// Mastodon's mention-label rule: `@user`, unless another resolved
/// mention shares the username under a different acct — then the
/// domain stays on every colliding label so the links remain
/// distinguishable.
fn mention_label(m: &MentionLink, mentions: &[MentionLink]) -> String {
    let user = m.acct.split('@').next().unwrap_or(m.acct.as_str());
    let ambiguous = mentions.iter().any(|o| {
        o.acct != m.acct
            && o.acct
                .split('@')
                .next()
                .map(|u| u.eq_ignore_ascii_case(user))
                .unwrap_or(false)
    });
    if ambiguous {
        format!("@{}", m.acct)
    } else {
        format!("@{user}")
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
        if c == b'#' || rest.starts_with('＃') {
            let marker_len = rest.chars().next().unwrap().len_utf8();
            if let Some(m) = HASHTAG_TOKEN_RE.find(rest) {
                let name_end = pos + m.end();
                let name = &rest[marker_len..m.end()];
                if name.chars().any(|ch| ch.is_alphabetic()) && boundary_ok(segment, pos) {
                    let normalized = name.to_lowercase();
                    // Mastodon's `class="mention hashtag" rel="tag"` markup
                    // — remote card scanners skip tagged anchors on both
                    // signals (pubby's anchor helper can't emit `class`).
                    out.push_str(&format!(
                        "<a href=\"{}\" class=\"mention hashtag\" rel=\"tag\">{}</a>",
                        html_escape(&hashtag_url(&normalized)),
                        html_escape(&segment[pos..name_end]),
                    ));
                    if !hashtags.contains(&normalized) {
                        hashtags.push(normalized);
                    }
                } else {
                    out.push_str(&segment[pos..name_end]);
                }
                pos = name_end;
            } else {
                // A `#`/`＃` not followed by a name char — emit the
                // marker alone and keep scanning.
                out.push_str(&rest[..marker_len]);
                pos += marker_len;
            }
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
                    // `class="u-url mention"` is what remote servers
                    // (and card scanners) use to spot mention anchors.
                    out.push_str(&format!(
                        "<a href=\"{}\" class=\"u-url mention\">{}</a>",
                        html_escape(&m.href),
                        html_escape(&mention_label(m, mentions)),
                    ));
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
    fn html_to_text_url_anchor_emits_href() {
        // Mastodon's invisible-span markup flattens to a broken
        // `scheme:// rest` — the href is the real URL.
        let html = "<p>see <a href=\"https://youtu.be/abc123\">\
                    <span class=\"invisible\">https://</span>\
                    <span>youtu.be/abc123</span></a> now</p>";
        assert_eq!(html_to_text(html), "see https://youtu.be/abc123 now");

        // Truncated ellipsis markup — still the full href.
        let html = "<p><a href=\"https://example.com/very/long/path\">\
                    <span class=\"invisible\">https://</span>\
                    <span class=\"ellipsis\">example.com/very/lon…</span></a></p>";
        assert_eq!(html_to_text(html), "https://example.com/very/long/path");

        // A non-http href never lands in the text.
        let html = "<p><a href=\"javascript:x\">https://example.com</a></p>";
        assert_eq!(html_to_text(html), "https://example.com");
    }

    #[test]
    fn html_to_text_descriptive_and_token_anchors() {
        // Descriptive text stays text — no URL leak into the
        // searchable content.
        let html = "<p>see <a href=\"https://example.com/a\">this article</a></p>";
        assert_eq!(html_to_text(html), "see this article");

        // Mention/hashtag anchors compact to their token.
        let html = "<p><a href=\"https://mas.to/@nemo\" class=\"mention\">\
                    @<span>nemo</span></a> said \
                    <a href=\"https://mas.to/tags/rust\" rel=\"tag\">\
                    #<span>rust</span></a></p>";
        assert_eq!(html_to_text(html), "@nemo said #rust");

        // Multi-word anchor text is not a token — kept verbatim.
        let html = "<p><a href=\"https://x.test\">@ our team</a></p>";
        assert_eq!(html_to_text(html), "@ our team");
    }

    #[test]
    fn mention_anchor_plain() {
        let mentions = vec![MentionLink {
            token: "@alice".to_string(),
            href: "https://y.test/@alice".to_string(),
            acct: "alice".to_string(),
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
            .contains("<a href=\"https://y.test/@alice\" class=\"u-url mention\">@alice</a>"));
    }

    #[test]
    fn mention_remote_label_drops_domain() {
        // Mastodon convention: the anchor text is `@user`, the domain
        // only lives in the href.
        let mentions = vec![MentionLink {
            token: "@alice@remote.example".to_string(),
            href: "https://remote.example/@alice".to_string(),
            acct: "alice@remote.example".to_string(),
        }];
        let r = render(
            "hey @alice@remote.example, look",
            "text/plain",
            &mentions,
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains(
            "<a href=\"https://remote.example/@alice\" class=\"u-url mention\">@alice</a>"
        ));
    }

    #[test]
    fn mention_label_keeps_domain_when_ambiguous() {
        // Same username on two domains — labels stay fully qualified.
        let mentions = vec![
            MentionLink {
                token: "@alice@a.example".to_string(),
                href: "https://a.example/@alice".to_string(),
                acct: "alice@a.example".to_string(),
            },
            MentionLink {
                token: "@alice@b.example".to_string(),
                href: "https://b.example/users/alice".to_string(),
                acct: "alice@b.example".to_string(),
            },
        ];
        let r = render(
            "cc @alice@a.example @alice@b.example",
            "text/plain",
            &mentions,
            "https://y.test",
            &cfg(),
        );
        assert!(r.html.contains(">@alice@a.example</a>"));
        assert!(r.html.contains(">@alice@b.example</a>"));
    }

    #[test]
    fn mention_inside_url_not_double_linked() {
        let mentions = vec![MentionLink {
            token: "@alice".to_string(),
            href: "https://y.test/@alice".to_string(),
            acct: "alice".to_string(),
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
    fn markdown_fence_keeps_language_class() {
        let r = render(
            "```python\ndef f():\n    pass\n```\n\n```\nplain\n```",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        // The language class is what clients highlight on; it must
        // survive the allowlist (class is otherwise stripped).
        assert!(r.html.contains("language-python"), "{}", r.html);
        assert!(r.html.contains("<pre><code"), "{}", r.html);
    }

    #[test]
    fn markdown_image_renders_inline_with_caption() {
        let r = render(
            "before ![Alt text](https://img.example/pic.png) after",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(
            r.html.contains(
                "<span data-yonder-inline-image><img src=\"https://img.example/pic.png\" alt=\"Alt text\"><small data-yonder-inline-image-alt><a href=\"https://img.example/pic.png\">Alt text</a></small></span>"
            ),
            "{}",
            r.html
        );
        // The caption links to the image exactly once — the image
        // itself isn't wrapped in an anchor.
        assert_eq!(
            r.html
                .matches("<a href=\"https://img.example/pic.png\"")
                .count(),
            1,
            "{}",
            r.html
        );
    }

    #[test]
    fn markdown_image_escapes_caption_and_rejects_bad_scheme() {
        let r = render(
            "![a <b> & 'q'](https://img.example/x.png) ![x](javascript:alert(1))",
            "text/markdown",
            &[],
            "https://y.test",
            &cfg(),
        );
        assert!(
            r.html.contains("a &lt;b&gt; &amp; &#x27;q&#x27;"),
            "{}",
            r.html
        );
        assert!(!r.html.contains("javascript:"), "{}", r.html);
        assert_eq!(
            markdown_images("![x](javascript:alert(1))", &cfg()).len(),
            0
        );
    }

    #[test]
    fn markdown_images_extracts_url_and_alt() {
        let images = markdown_images(
            "![one](https://a.example/1.png) text ![two *em*](https://a.example/2.jpg) ![one](https://a.example/1.png)",
            &cfg(),
        );
        assert_eq!(
            images,
            vec![
                InlineImage {
                    url: "https://a.example/1.png".to_string(),
                    alt: "one".to_string(),
                },
                InlineImage {
                    url: "https://a.example/2.jpg".to_string(),
                    alt: "two em".to_string(),
                },
            ]
        );
        // Code spans never produce images.
        assert!(markdown_images("`![x](https://a.example/x.png)`", &cfg()).is_empty());
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
    fn unicode_hashtags_linkify_and_encode() {
        // Mastodon's `Tag::HASHTAG_NAME_PAT` — letters/marks/digits in
        // any script, `·`/ZWNJ mid-tag, `＃` marker, names end on a
        // word char. Markdown path (enhance_segment) and plain path
        // (pubby render_post_html) agree.
        for content_type in ["text/markdown", "text/plain"] {
            let r = render(
                "Ich #Scheinkünstler und #हिंदी, #a·b, #tag· rest #123",
                content_type,
                &[],
                "https://y.test",
                &cfg(),
            );
            for tag in ["scheinkünstler", "हिंदी", "a·b", "tag"] {
                assert!(
                    r.hashtags.contains(&tag.to_string()),
                    "{content_type}: {tag}"
                );
            }
            assert!(
                r.html.contains("/tags/scheink%C3%BCnstler"),
                "{content_type}"
            );
            assert!(r.html.contains("#tag</a>· rest #123"), "{content_type}");
        }
    }

    #[test]
    fn cw_is_escaped() {
        assert_eq!(render_spoiler("<b>x</b>"), "&lt;b&gt;x&lt;/b&gt;");
    }

    #[test]
    fn remote_html_keeps_attachment_backed_image_inline() {
        let url = "https://img.example/pic.png";
        let allowed: HashSet<String> = [url.to_string()].into_iter().collect();
        let (html, kept) = render_remote_html(
            &format!("<p>look <img src=\"{url}\" alt=\"a cat\"></p>"),
            &cfg(),
            &allowed,
            false,
        );
        assert!(
            html.contains(
                "<span data-yonder-inline-image><img src=\"https://img.example/pic.png\" alt=\"a cat\"><small data-yonder-inline-image-alt><a href=\"https://img.example/pic.png\">a cat</a></small></span>"
            ),
            "{html}"
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn remote_html_drops_unattached_image() {
        let (html, kept) = render_remote_html(
            "<p><img src=\"https://t.example/px.png\"></p>",
            &cfg(),
            &HashSet::new(),
            false,
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("data-yonder-inline-image"), "{html}");
        assert!(kept.is_empty());
    }

    #[test]
    fn remote_html_allow_all_keeps_images() {
        // Converted long-form objects (Friendica articles) carry
        // images only in `content` — no attachment to match against.
        let (html, kept) = render_remote_html(
            "<p><img src=\"https://img.example/a.png\" alt=\"x\"></p>",
            &cfg(),
            &HashSet::new(),
            true,
        );
        assert!(html.contains("data-yonder-inline-image"), "{html}");
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn remote_html_own_markup_does_not_duplicate_caption() {
        // A remote Yonder post round-trips our wrapper markup; it must
        // collapse back to one inline image + one caption.
        let url = "https://img.example/pic.png";
        let allowed: HashSet<String> = [url.to_string()].into_iter().collect();
        let src = format!(
            "<p><span data-yonder-inline-image><img src=\"{url}\" alt=\"cap\"><small data-yonder-inline-image-alt>cap</small></span></p>"
        );
        let (html, _) = render_remote_html(&src, &cfg(), &allowed, false);
        assert_eq!(
            html.matches("<span data-yonder-inline-image>").count(),
            1,
            "{html}"
        );
        assert_eq!(
            html.matches("data-yonder-inline-image-alt").count(),
            1,
            "{html}"
        );
    }

    #[test]
    fn normalize_url_for_match_handles_trivial_differences() {
        let a = normalize_url_for_match("https://Ex.com:443/a/b/").unwrap();
        let b = normalize_url_for_match("https://ex.com/a/b").unwrap();
        assert_eq!(a, b);
        assert!(normalize_url_for_match("javascript:alert(1)").is_none());
        assert!(normalize_url_for_match("/relative.png").is_none());
    }

    #[test]
    fn inline_image_srcs_collects_normalized_urls() {
        let srcs = inline_image_srcs(
            "<span data-yonder-inline-image><img src=\"https://a.example/x.png\" alt=\"\"></span><p>no img</p>",
        );
        assert_eq!(srcs.len(), 1);
        assert!(srcs.contains("https://a.example/x.png"));
    }

    // -- math source extraction ----------------------------------------

    fn math_cfg() -> Config {
        let mut c = cfg();
        c.math.enabled = true;
        c.math.command = "true".to_string();
        c
    }

    #[test]
    fn markdown_multiline_display_math_survives() {
        // Regression: pulldown-cmark eats backslash escapes (`\\` → `\`,
        // `\,` → `,`) — extraction must happen on the raw source so
        // `aligned` row separators and `\\[4pt]` spacing reach the
        // renderer verbatim.
        let src = "intro\n$$\n\\begin{aligned}\n\\text{(A1)}\\quad & a \\\\\n b &= c \\\\[4pt]\n\\end{aligned}\n$$\noutro";
        let r = render(src, "text/markdown", &[], "https://y.test", &math_cfg());
        assert!(
            r.html.contains("data-yonder-math=\"display\""),
            "{}",
            r.html
        );
        assert!(r.html.contains("\\begin{aligned}"), "{}", r.html);
        assert!(r.html.contains("\\\\"), "row sep lost: {}", r.html);
        assert!(r.html.contains("\\\\[4pt]"), "spacing lost: {}", r.html);
        assert!(r.html.contains("intro"), "{}", r.html);
        assert!(r.html.contains("outro"), "{}", r.html);
    }

    #[test]
    fn plain_multiline_display_math_survives() {
        // Regression: the plain-text renderer inserts `<br>` per line,
        // splitting `$$…$$` across text nodes so the HTML scanner
        // never matched. Extraction on the source fixes it.
        let src = "x\n$$\n\\begin{aligned}\na &= b \\\\\n\\end{aligned}\n$$\ny";
        let r = render(src, "text/plain", &[], "https://y.test", &math_cfg());
        assert!(
            r.html.contains("data-yonder-math=\"display\""),
            "{}",
            r.html
        );
        assert!(r.html.contains("\\begin{aligned}"), "{}", r.html);
    }

    #[test]
    fn markdown_math_in_code_links_stays_literal() {
        // Same exclusions as the HTML scanner: code spans and link
        // text/destinations keep `$…$` literal; only the bare `$z$`
        // becomes a placeholder.
        let src = "`$x$` and [$y$](https://e.com) and $z$";
        let r = render(src, "text/markdown", &[], "https://y.test", &math_cfg());
        assert_eq!(
            r.html.matches("data-yonder-math=\"").count(),
            1,
            "{}",
            r.html
        );
        assert!(r.html.contains("<code>$x$</code>"), "{}", r.html);
        assert!(r.html.contains(">$y$</a>"), "{}", r.html);
        assert!(r.html.contains("data-yonder-math-src=\"z\""), "{}", r.html);
    }

    #[test]
    fn markdown_disabled_math_renders_normally() {
        // With [math] off the source is untouched — Markdown still
        // eats the backslashes (pre-existing behaviour).
        let src = "$$\na \\\\ b\n$$";
        let r = render(src, "text/markdown", &[], "https://y.test", &cfg());
        assert!(!r.html.contains("data-yonder-math"), "{}", r.html);
    }
}
