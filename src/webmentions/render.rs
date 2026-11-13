//! Render the Microformats2 `h-entry` source page for outgoing
//! Webmentions — mirrors Songhive's `webmentions/render.py`.
//!
//! Remote receivers fetch this document to verify and display a
//! mention delivered for a local status. The markup deliberately
//! mirrors what the incoming parser understands: an `h-entry` with
//! `u-url`/`dt-published`/`p-author` (h-card), `u-in-reply-to` when the
//! status is a reply, the post's sanitized HTML as `e-content`, its
//! plain text as `p-summary`, and `p-category` links for its tags.
//! The `<link rel="webmention">` element advertises our own endpoint
//! to senders who scan the page too.

use crate::posts::{render::hashtag_url, StatusRow};

/// The author fragment of the source page (an `h-card`).
pub struct AuthorCard {
    /// `/@username` profile URL (absolute).
    pub profile_url: String,
    /// Display name or username.
    pub name: String,
    /// Absolute avatar URL (optional).
    pub photo: Option<String>,
    /// Bare username (used to build the status permalink).
    pub username: String,
    /// `users.indexable = false` — the document gets a robots
    /// `noindex` meta so crawlers honour the author's opt-out.
    pub noindex: bool,
}

/// The remote permalinks a status interacts with, rendered as the
/// typed mf2 links receivers use to classify the mention
/// (`in-reply-to`, `repost-of`, `quotation-of`). All `None` for an
/// original post — backfeed only happens toward remote targets.
#[derive(Default)]
pub struct Interactions {
    /// `u-in-reply-to` — the parent status's remote permalink.
    pub reply: Option<String>,
    /// `u-repost-of` — the boosted status's remote permalink.
    pub repost: Option<String>,
    /// `u-quotation-of` — the quoted status's remote permalink.
    pub quote: Option<String>,
}

impl Interactions {
    /// Any remote interaction link present.
    pub fn any(&self) -> bool {
        self.reply.is_some() || self.repost.is_some() || self.quote.is_some()
    }
}

fn esc(s: &str) -> String {
    pubby_sdk::content::html_escape(s)
}

/// Render the `h-entry` HTML document served as a status's outgoing
/// mention source.
///
/// `status_url` is the public permalink; `interactions` the remote
/// permalinks this status replies to, reposts or quotes; `tags` the
/// status's hashtag names.
pub fn render_source_page(
    base_url: &str,
    endpoint_url: &str,
    status: &StatusRow,
    status_url: &str,
    interactions: &Interactions,
    author: Option<&AuthorCard>,
    tags: &[String],
) -> String {
    let summary = crate::posts::render::html_to_text(
        status.content_html.as_deref().unwrap_or(&status.content),
    );

    let mut parts = vec![
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\" />".to_string(),
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0, viewport-fit=cover\" />".to_string(),
        "<style>body{width:100vw;height:100vh;background:#f0f0f0;display:flex;overflow:hidden}\
        .h-entry{background:white;display:flex;flex-direction:column;margin:auto;padding:20px;\
        gap:10px;overflow:hidden;border:1px solid #ddd;border-radius:10px}\
        .p-author{display:flex;align-items:center;gap:7.5px}\
        .u-photo{width:32px;height:32px;border-radius:50%}\
        .dt-published{font-size:.85em;color:#666}\
        a,a:visited{color:#009;text-decoration:none}a:hover{text-decoration:underline}\
        .hidden{display:none}</style>".to_string(),
        format!(
            "<link rel=\"webmention\" href=\"{}\" />",
            esc(endpoint_url)
        ),
        // The author's search-engine opt-out applies to this document
        // too — it carries their content verbatim.
        author
            .filter(|a| a.noindex)
            .map(|_| "<meta name=\"robots\" content=\"noindex\" />".to_string())
            .unwrap_or_default(),
        format!("<title>{}</title>", esc(&summary.chars().take(80).collect::<String>())),
        "</head><body>".to_string(),
        "<article class=\"h-entry\">".to_string(),
    ];

    if let Some(author) = author {
        let photo = author
            .photo
            .as_deref()
            .map(|p| format!("<img class=\"u-photo\" src=\"{}\" alt=\"\" />", esc(p)))
            .unwrap_or_default();
        parts.push(format!(
            "<span class=\"p-author h-card\">{photo}\
             <a class=\"u-url\" href=\"{}\"><span class=\"p-name\">{}</span></a></span>",
            esc(&author.profile_url),
            esc(&author.name),
        ));
    }
    if let Some(reply) = &interactions.reply {
        parts.push(format!(
            "<a class=\"u-in-reply-to\" href=\"{}\">In reply to</a>",
            esc(reply)
        ));
    }
    if let Some(repost) = &interactions.repost {
        parts.push(format!(
            "<a class=\"u-repost-of\" href=\"{}\">Repost of</a>",
            esc(repost)
        ));
    }
    if let Some(quote) = &interactions.quote {
        parts.push(format!(
            "<a class=\"u-quotation-of\" href=\"{}\">Quote of</a>",
            esc(quote)
        ));
    }
    if !summary.is_empty() {
        let classes = if status.content_html.is_some() {
            "p-summary hidden"
        } else {
            "p-summary"
        };
        parts.push(format!("<p class=\"{classes}\">{}</p>", esc(&summary)));
    }
    parts.push(format!(
        "<a class=\"u-url u-uid\" href=\"{}\">Status URL</a>",
        esc(status_url)
    ));
    parts.push(format!(
        "<time class=\"dt-published\" datetime=\"{}\">at {}</time>",
        esc(&status.created_at.to_rfc3339()),
        esc(&status.created_at.format("%Y-%m-%d %H:%M").to_string()),
    ));
    if let Some(html) = status.content_html.as_deref().filter(|h| !h.is_empty()) {
        // Server-rendered and sanitized at publish time — verbatim.
        parts.push(format!("<div class=\"e-content\">{html}</div>"));
    }
    let tag_url = hashtag_url(base_url);
    for name in tags {
        parts.push(format!(
            "<a class=\"p-category\" href=\"{}\">{}</a>",
            esc(&tag_url(name)),
            esc(name),
        ));
    }
    parts.push("</article></body></html>".to_string());
    parts.concat()
}

/// Render the `u-like-of` source document served as a favourite's
/// mention source — remote receivers verify the like by finding the
/// `u-like-of` link to their post (`target_url`) here. `like_url` is
/// the document's own permalink, `liker` the h-card of the account
/// that favourited, `liked_at` the favourite timestamp.
pub fn render_like_page(
    endpoint_url: &str,
    like_url: &str,
    target_url: &str,
    liker: &AuthorCard,
    liked_at: &chrono::DateTime<chrono::Utc>,
) -> String {
    let photo = liker
        .photo
        .as_deref()
        .map(|p| format!("<img class=\"u-photo\" src=\"{}\" alt=\"\" />", esc(p)))
        .unwrap_or_default();
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\" />\
         <link rel=\"webmention\" href=\"{}\" />\
         {}\
         <title>{} liked {}</title></head><body>\
         <article class=\"h-entry\">\
         <span class=\"p-author h-card\">{photo}\
         <a class=\"u-url\" href=\"{}\"><span class=\"p-name\">{}</span></a></span>\
         <a class=\"u-like-of\" href=\"{}\">liked this</a>\
         <a class=\"u-url u-uid\" href=\"{}\">Like</a>\
         <time class=\"dt-published\" datetime=\"{}\"></time>\
         </article></body></html>",
        esc(endpoint_url),
        if liker.noindex {
            "<meta name=\"robots\" content=\"noindex\" />"
        } else {
            ""
        },
        esc(&liker.name),
        esc(target_url),
        esc(&liker.profile_url),
        esc(&liker.name),
        esc(target_url),
        esc(like_url),
        esc(&liked_at.to_rfc3339()),
    )
}
