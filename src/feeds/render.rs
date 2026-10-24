//! RSS 2.0 and Atom 1.0 feed serialization (epic 19).
//!
//! Pure functions: [`Feed`]/[`FeedItem`] in, a well-formed XML document
//! out. Every dynamic string — text nodes and attribute values alike —
//! passes through [`quick_xml::escape::escape`], so hostile content can
//! never break out of its element. Both renderers take `now` explicitly
//! so output is deterministic.

use std::borrow::Cow;

use chrono::{DateTime, SecondsFormat, Utc};
use quick_xml::escape::escape;

/// `Content-Type` header for Atom documents.
pub const ATOM_CONTENT_TYPE: &str = "application/atom+xml; charset=utf-8";
/// `Content-Type` header for RSS documents.
pub const RSS_CONTENT_TYPE: &str = "application/rss+xml; charset=utf-8";

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const GENERATOR: &str = "Yonder";

/// A single feed entry.
#[derive(Debug, Clone)]
pub struct FeedItem {
    /// Stable id (the status URI).
    pub id: String,
    /// Entry title — spoiler text or a truncated content preview.
    pub title: String,
    /// Permalink to the item.
    pub link: String,
    /// Publication timestamp (`statuses.created_at`).
    pub published: DateTime<Utc>,
    /// Last-change timestamp (`edited_at`, else `created_at`).
    pub updated: DateTime<Utc>,
    /// Display name or `@{username}` of the author.
    pub author: String,
    /// Sanitized post HTML (`statuses.content_html`).
    pub content_html: String,
}

/// A feed document ready for serialization.
#[derive(Debug, Clone)]
pub struct Feed {
    /// Stable feed id (the linked resource's URL).
    pub id: String,
    /// Feed title (display name, `#tag` or list name).
    pub title: String,
    /// The resource this feed describes.
    pub link: String,
    /// Absolute URL of the feed itself (`rel="self"`).
    pub self_url: String,
    /// Optional description (RSS channel element).
    pub description: Option<String>,
    /// Timestamp of the newest item — `None` on an empty feed.
    pub updated: Option<DateTime<Utc>>,
    pub items: Vec<FeedItem>,
}

/// Drop characters XML 1.0 forbids, even as entities. One stray control
/// byte from user input would otherwise make the whole feed unparseable.
pub fn xml_safe(s: &str) -> Cow<'_, str> {
    if s.chars().all(is_xml_char) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.chars().filter(|c| is_xml_char(*c)).collect())
    }
}

fn is_xml_char(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
    )
}

/// `<tag>text</tag>` or `<tag attr="v" />` — always escaped.
fn el(tag: &str, attrs: &[(&str, &str)], text: Option<&str>) -> String {
    let attrs: String = attrs
        .iter()
        .map(|(n, v)| format!(" {n}=\"{}\"", escape(xml_safe(v))))
        .collect();
    match text {
        None => format!("<{tag}{attrs} />"),
        Some(t) => format!("<{tag}{attrs}>{}</{tag}>", escape(xml_safe(t))),
    }
}

fn rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Serialize `feed` as an Atom 1.0 document.
pub fn render_atom(feed: &Feed, now: DateTime<Utc>) -> String {
    let updated = feed.updated.unwrap_or(now);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!("<feed xmlns=\"{ATOM_NS}\">"));
    out.push_str(&el("id", &[], Some(&feed.id)));
    out.push_str(&el("title", &[], Some(&feed.title)));
    out.push_str(&el(
        "link",
        &[("href", &feed.link), ("rel", "alternate")],
        None,
    ));
    out.push_str(&el(
        "link",
        &[
            ("href", &feed.self_url),
            ("rel", "self"),
            ("type", "application/atom+xml"),
        ],
        None,
    ));
    out.push_str(&el("updated", &[], Some(&rfc3339(updated))));
    out.push_str(&el("generator", &[], Some(GENERATOR)));

    for item in &feed.items {
        out.push_str("<entry>");
        out.push_str(&el("id", &[], Some(&item.id)));
        out.push_str(&el("title", &[], Some(&item.title)));
        out.push_str(&el(
            "link",
            &[("href", &item.link), ("rel", "alternate")],
            None,
        ));
        out.push_str(&el("published", &[], Some(&rfc3339(item.published))));
        out.push_str(&el("updated", &[], Some(&rfc3339(item.updated))));
        out.push_str(&format!(
            "<author>{}</author>",
            el("name", &[], Some(&item.author))
        ));
        out.push_str(&el(
            "content",
            &[("type", "html")],
            Some(&item.content_html),
        ));
        out.push_str("</entry>");
    }

    out.push_str("</feed>");
    out
}

/// Serialize `feed` as an RSS 2.0 document.
pub fn render_rss(feed: &Feed, now: DateTime<Utc>) -> String {
    let updated = feed.updated.unwrap_or(now);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!(
        "<rss version=\"2.0\" xmlns:atom=\"{ATOM_NS}\" xmlns:dc=\"{DC_NS}\">"
    ));
    out.push_str("<channel>");
    out.push_str(&el("title", &[], Some(&feed.title)));
    out.push_str(&el("link", &[], Some(&feed.link)));
    let description = feed.description.as_deref().unwrap_or(&feed.title);
    out.push_str(&el("description", &[], Some(description)));
    out.push_str(&el(
        "atom:link",
        &[
            ("href", &feed.self_url),
            ("rel", "self"),
            ("type", "application/rss+xml"),
        ],
        None,
    ));
    out.push_str(&el("lastBuildDate", &[], Some(&updated.to_rfc2822())));

    for item in &feed.items {
        out.push_str("<item>");
        out.push_str(&el("title", &[], Some(&item.title)));
        out.push_str(&el("link", &[], Some(&item.link)));
        out.push_str(&el("guid", &[("isPermaLink", "false")], Some(&item.id)));
        out.push_str(&el("pubDate", &[], Some(&item.published.to_rfc2822())));
        out.push_str(&el("description", &[], Some(&item.content_html)));
        out.push_str(&el("dc:creator", &[], Some(&item.author)));
        out.push_str("</item>");
    }

    out.push_str("</channel></rss>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn item(id: &str, title: &str) -> FeedItem {
        FeedItem {
            id: id.to_string(),
            title: title.to_string(),
            link: "https://example.com/@alice/1".to_string(),
            published: ts(1_700_000_000),
            updated: ts(1_700_000_100),
            author: "Alice".to_string(),
            content_html: "<p>hello &lt;world&gt; <script>alert(1)</script></p>".to_string(),
        }
    }

    fn feed(items: Vec<FeedItem>) -> Feed {
        Feed {
            id: "https://example.com/@alice".to_string(),
            title: "Alice <The Great> & \"Friends\"".to_string(),
            link: "https://example.com/@alice".to_string(),
            self_url: "https://example.com/users/alice.atom".to_string(),
            description: Some("Alice's posts".to_string()),
            updated: items.first().map(|i| i.published),
            items,
        }
    }

    /// Assert the document parses cleanly with quick-xml.
    fn assert_well_formed(xml: &str) {
        let mut reader = Reader::from_str(xml);
        while !matches!(reader.read_event().expect("well-formed XML"), Event::Eof) {}
    }

    #[test]
    fn atom_required_elements() {
        let xml = render_atom(&feed(vec![item("u:1", "first")]), ts(1_700_100_000));
        assert_well_formed(&xml);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
        for needle in [
            "<id>https://example.com/@alice</id>",
            "<link href=\"https://example.com/users/alice.atom\" rel=\"self\" type=\"application/atom+xml\" />",
            "<generator>Yonder</generator>",
            "<published>2023-11-14T22:13:20Z</published>",
            "<updated>2023-11-14T22:15:00Z</updated>",
            "<author><name>Alice</name></author>",
            "<content type=\"html\">",
        ] {
            assert!(xml.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn rss_required_elements() {
        let xml = render_rss(&feed(vec![item("u:1", "first")]), ts(1_700_100_000));
        assert_well_formed(&xml);
        for needle in [
            "<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">",
            "<channel>",
            "<title>",
            "<link>",
            "<description>",
            "<guid isPermaLink=\"false\">u:1</guid>",
            "<pubDate>Tue, 14 Nov 2023 22:13:20",
            "<dc:creator>Alice</dc:creator>",
        ] {
            assert!(xml.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn escaping_is_complete() {
        let f = feed(vec![item("u:1", "t < & \" ' 💥")]);
        let atom = render_atom(&f, ts(0));
        assert_well_formed(&atom);
        assert!(!atom.contains("Alice <The Great>"));
        assert!(atom.contains("Alice &lt;The Great&gt; &amp; &quot;Friends&quot;"));
        assert!(atom.contains("t &lt; &amp; &quot; &apos; 💥"));
        assert!(!atom.contains("<script>"));
        let rss = render_rss(&f, ts(0));
        assert_well_formed(&rss);
        assert!(rss.contains("t &lt; &amp; &quot; &apos; 💥"));
        assert!(!rss.contains("<script>"));
    }

    #[test]
    fn strips_xml_invalid_characters() {
        let xml = render_atom(&feed(vec![item("u:1", "a\u{1}b\u{FFFE}c\u{B}")]), ts(0));
        assert_well_formed(&xml);
        assert!(xml.contains("<title>abc</title>"));
        assert_eq!(xml_safe("tab\tnl\ncr\r"), "tab\tnl\ncr\r");
    }

    #[test]
    fn rss_escaping_in_attributes_and_text() {
        let mut f = feed(vec![]);
        f.self_url = "https://example.com/f.atom?x=1&y=\"2\"".to_string();
        let xml = render_rss(&f, ts(0));
        assert_well_formed(&xml);
        assert!(xml.contains("x=1&amp;y=&quot;2&quot;"));
    }

    #[test]
    fn empty_feed_falls_back_to_now() {
        let xml = render_atom(&feed(vec![]), ts(1_700_100_000));
        assert_well_formed(&xml);
        assert!(xml.contains("<updated>2023-11-16T02:00:00Z</updated>"));
        assert!(!xml.contains("<entry>"));
    }

    #[test]
    fn renderers_are_deterministic() {
        let f = feed(vec![item("u:1", "a")]);
        assert_eq!(render_atom(&f, ts(1)), render_atom(&f, ts(1)));
        assert_eq!(render_rss(&f, ts(1)), render_rss(&f, ts(1)));
    }
}
