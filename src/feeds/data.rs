//! Feed data assembly (epic 19): statuses read through the shared
//! timeline query path become [`Feed`]/[`FeedItem`] values for
//! [`super::render`].
//!
//! Feeds are bounded "latest N" reads — a single `Page` with no cursors,
//! never `paginate::parse` (its 1..=40 clamp would silently cap the
//! configured `content.max_feed_items`).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::{
    http::paginate::Page,
    posts::{render::html_to_text, StatusRow},
    social::lists::ListRow,
    timelines::{
        query,
        scope::{RepliesPolicy, TimelineScope},
        Filters,
    },
    users::account::{self as account_mapper, AccountRow},
    Result,
};

use super::render::{Feed, FeedItem};

/// Item titles are plain-text previews; longer contents truncate.
const TITLE_MAX_CHARS: usize = 100;

/// The account's latest **public** posts (`TimelineScope::Account`
/// tightened by [`Filters::public_only`]). Boosts are excluded — a boost
/// row has no content of its own.
pub async fn account_feed(
    db: &PgPool,
    base_url: &str,
    account: &AccountRow,
    max_items: i64,
) -> Result<Feed> {
    let scope = TimelineScope::Account {
        account_id: account.id,
    };
    let rows = fetch(db, &scope, public_filters(), max_items).await?;
    let url = format!("{base_url}/@{}", account.username);
    let entries = items(db, &rows).await?;
    Ok(Feed {
        id: url.clone(),
        title: account
            .display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(account.username.as_str())
            .to_string(),
        link: url,
        self_url: String::new(),
        description: None,
        updated: newest(&entries),
        items: entries,
    })
}

/// The latest public posts carrying `tag` (case-insensitive). An unknown
/// tag is a valid empty feed, not an error.
pub async fn tag_feed(db: &PgPool, base_url: &str, tag: &str, max_items: i64) -> Result<Feed> {
    let scope = TimelineScope::Tag {
        name: tag.to_lowercase(),
        local_only: false,
    };
    let rows = fetch(db, &scope, public_filters(), max_items).await?;
    let url = tag_url(base_url, tag);
    let entries = items(db, &rows).await?;
    Ok(Feed {
        id: url.clone(),
        title: format!("#{tag}"),
        link: url,
        self_url: String::new(),
        description: Some(format!("Public posts tagged #{tag}")),
        updated: newest(&entries),
        items: entries,
    })
}

/// `{base}/tags/{tag}` with the tag percent-encoded as one path segment.
/// `+` is never a literal here: `byte_serialize` encodes it as `%2B`, so
/// the space-to-`+` mapping is safely undone.
pub fn tag_url(base_url: &str, tag: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect();
    format!("{base_url}/tags/{}", encoded.replace('+', "%20"))
}

/// The list's posts for its owner — `viewer` is the owner, so the list
/// scope admits members' `followers_only`/`private` posts but never
/// `direct`. `replies_policy` applies as on the API timeline. Boosts are
/// excluded — a boost row has no content of its own.
pub async fn list_feed(
    db: &PgPool,
    base_url: &str,
    list: &ListRow,
    max_items: i64,
) -> Result<Feed> {
    let scope = TimelineScope::List {
        list_id: list.id,
        replies_policy: RepliesPolicy::parse(&list.replies_policy),
    };
    let filters = Filters {
        viewer: Some(list.account_id),
        exclude_reblogs: true,
        ..Default::default()
    };
    let rows = fetch(db, &scope, filters, max_items).await?;
    let url = format!("{base_url}/lists/{}", list.id);
    let entries = items(db, &rows).await?;
    Ok(Feed {
        id: url.clone(),
        title: list.title.clone(),
        link: url,
        self_url: String::new(),
        description: Some(format!("Posts from \"{}\"", list.title)),
        updated: newest(&entries),
        items: entries,
    })
}

/// Anonymous-feed filters: strictly public, boosts out.
fn public_filters() -> Filters {
    Filters {
        viewer: None,
        public_only: true,
        exclude_reblogs: true,
        ..Default::default()
    }
}

async fn fetch(
    db: &PgPool,
    scope: &TimelineScope,
    filters: Filters,
    max_items: i64,
) -> Result<Vec<StatusRow>> {
    let page = Page {
        max_id: None,
        since_id: None,
        limit: max_items,
    };
    query::fetch(db, scope, &filters, &page).await
}

/// Map status rows to feed items, resolving authors in one query.
async fn items(db: &PgPool, rows: &[StatusRow]) -> Result<Vec<FeedItem>> {
    let mut ids: Vec<i64> = rows.iter().map(|r| r.account_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let authors: HashMap<i64, AccountRow> = account_mapper::load_many(db, &ids)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();

    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.uri.clone()?;
            let author = authors
                .get(&row.account_id)
                .map(author_name)
                .unwrap_or_default();
            Some(FeedItem {
                link: item_link(row.url.as_deref(), &id),
                id,
                title: item_title(row),
                published: row.created_at,
                updated: row.edited_at.unwrap_or(row.created_at),
                author,
                content_html: item_content(row),
            })
        })
        .collect())
}

/// Feed-level timestamp: the newest item's `updated`, `None` when empty.
fn newest(items: &[FeedItem]) -> Option<DateTime<Utc>> {
    items.iter().map(|item| item.updated).max()
}

/// Permalink, falling back to the status URI. Boost rows store `url = ''`
/// rather than NULL, so an empty URL falls back too.
fn item_link(url: Option<&str>, uri: &str) -> String {
    url.filter(|url| !url.is_empty()).unwrap_or(uri).to_string()
}

/// Stored sanitized HTML, or the escaped plain content for rows that
/// never got a render.
fn item_content(row: &StatusRow) -> String {
    row.content_html
        .clone()
        .unwrap_or_else(|| pubby_sdk::content::html_escape(&row.content))
}

/// `spoiler_text` when present, else a plain-text preview truncated to
/// [`TITLE_MAX_CHARS`] characters (never splitting a multi-byte char).
fn item_title(row: &StatusRow) -> String {
    if let Some(spoiler) = &row.spoiler_text {
        if !spoiler.is_empty() {
            return spoiler.clone();
        }
    }
    let text = match &row.content_html {
        Some(html) => html_to_text(html),
        None => row.content.clone(),
    };
    truncate_chars(&text, TITLE_MAX_CHARS)
}

/// `display_name`, else `@{acct}` (`@user@domain` for remote authors —
/// the same acct shape `to_account` emits).
fn author_name(account: &AccountRow) -> String {
    if let Some(name) = &account.display_name {
        if !name.is_empty() {
            return name.clone();
        }
    }
    match &account.domain {
        Some(domain) => format!("@{}@{domain}", account.username),
        None => format!("@{}", account.username),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut iter = text.char_indices();
    match iter.nth(max) {
        Some((byte, _)) => format!("{}…", &text[..byte]),
        None => text.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn status(content: &str, content_html: Option<&str>, spoiler: Option<&str>) -> StatusRow {
        StatusRow {
            id: 1,
            account_id: 1,
            content: content.to_string(),
            content_html: content_html.map(str::to_string),
            content_type: "text/plain".to_string(),
            language: None,
            visibility: "public".to_string(),
            spoiler_text: spoiler.map(str::to_string),
            sensitive: false,
            local: true,
            uri: Some("u:1".to_string()),
            url: Some("https://example.com/@a/1".to_string()),
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            reblog_of_id: None,
            quote_of_id: None,
            quote_uri: None,
            poll_id: None,
            replies_count: 0,
            reblogs_count: 0,
            favourites_count: 0,
            quotes_count: 0,
            edited_at: None,
            tombstone: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn title_prefers_spoiler_text() {
        let row = status("body", None, Some("cw here"));
        assert_eq!(item_title(&row), "cw here");
        let empty = status("body", None, Some(""));
        assert_eq!(item_title(&empty), "body");
    }

    #[test]
    fn title_derives_plain_text_from_html() {
        let row = status("raw", Some("<p>hi <b>there</b></p>"), None);
        assert_eq!(item_title(&row), "hi there");
    }

    #[test]
    fn title_truncates_on_char_boundary() {
        let long = "a".repeat(TITLE_MAX_CHARS) + "💥more";
        let row = status(&long, None, None);
        let title = item_title(&row);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));

        let multi = "💥".repeat(TITLE_MAX_CHARS + 5);
        let row = status(&multi, None, None);
        let title = item_title(&row);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
    }

    fn account(username: &str, domain: Option<&str>, display_name: Option<&str>) -> AccountRow {
        AccountRow {
            id: 1,
            username: username.to_string(),
            domain: domain.map(str::to_string),
            actor_id: String::new(),
            url: None,
            display_name: display_name.map(str::to_string),
            bio: None,
            locked: false,
            bot: false,
            discoverable: false,
            avatar_path: None,
            banner_path: None,
            avatar_remote_url: None,
            banner_remote_url: None,
            avatar_blurhash: None,
            header_blurhash: None,
            profile_fields: serde_json::json!([]),
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            indexable: None,
            suspended_at: None,
            moved_to_account_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn link_falls_back_to_uri_when_url_missing_or_empty() {
        assert_eq!(item_link(Some("https://a/1"), "u:1"), "https://a/1");
        assert_eq!(item_link(Some(""), "u:1"), "u:1");
        assert_eq!(item_link(None, "u:1"), "u:1");
    }

    #[test]
    fn content_falls_back_to_escaped_plain_text() {
        let rendered = status("raw", Some("<p>ok</p>"), None);
        assert_eq!(item_content(&rendered), "<p>ok</p>");
        let plain = status("a<b>&\"c\"", None, None);
        assert_eq!(item_content(&plain), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn tag_url_percent_encodes_one_segment() {
        assert_eq!(tag_url("https://a", "rust"), "https://a/tags/rust");
        assert_eq!(tag_url("https://a", "a b"), "https://a/tags/a%20b");
        assert_eq!(
            tag_url("https://a", "x?y#z/q+r"),
            "https://a/tags/x%3Fy%23z%2Fq%2Br"
        );
    }

    #[test]
    fn newest_is_max_item_update() {
        let at = |secs| Utc.timestamp_opt(secs, 0).unwrap();
        let item = |updated| FeedItem {
            id: String::new(),
            title: String::new(),
            link: String::new(),
            published: at(0),
            updated,
            author: String::new(),
            content_html: String::new(),
        };
        assert_eq!(
            newest(&[item(at(100)), item(at(300)), item(at(50))]),
            Some(at(300))
        );
        assert_eq!(newest(&[]), None);
    }

    #[test]
    fn author_falls_back_to_acct() {
        assert_eq!(author_name(&account("alice", None, None)), "@alice");
        assert_eq!(
            author_name(&account("bob", Some("remote.example"), None)),
            "@bob@remote.example"
        );
        assert_eq!(
            author_name(&account("alice", None, Some("Alice A."))),
            "Alice A."
        );
    }
}
