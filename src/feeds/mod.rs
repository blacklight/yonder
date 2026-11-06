//! RSS/Atom feeds for profiles, hashtags and lists (epic 19).
//!
//! Mastodon-shaped URLs (`/users/{u}.atom`, `/tags/{t}.rss`) can't be
//! expressed as a single axum path parameter, so the routes are
//! `{*rest}` catch-alls and [`split_feed_path`] parses the extension.
//! Profile and hashtag feeds carry public posts only; list feeds are
//! authenticated and serve the owner's list view (`Bearer` or
//! `?access_token=`, `read:lists` scope). Everything rate-limits on the
//! `feed` bucket.

pub mod data;
pub mod render;

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use chrono::Utc;
use quick_xml::escape::escape;

use crate::{
    auth::scopes::require_scope,
    error::Error,
    http::{
        frontend::{frontend_choice, spa_entry},
        middleware::{auth::MaybeAuth, ratelimit},
        AppState,
    },
    posts::PostContext,
    social,
    users::account as account_mapper,
    Result,
};

/// Feeds share the app-wide router but not `/api` — merged
/// unconditionally (no federation requirement), outside the OpenAPI doc.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/users/{*rest}", get(profile_feed))
        .route("/tags/{*rest}", get(tag_feed))
        .route("/lists/{*rest}", get(list_feed))
        .route_layer(ratelimit::layer(state, "feed"))
}

/// The serialisation a feed path asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Atom,
    Rss,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Atom => "atom",
            Format::Rss => "rss",
        }
    }
}

/// Split `{*rest}` into the resource name and the requested format.
/// Only a single segment ending in exactly `.atom` or `.rss` resolves —
/// anything else (multi-segment remainders, unknown or missing
/// extensions, empty names) is `None` → 404.
fn split_feed_path(rest: &str) -> Option<(String, Format)> {
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let (name, format) = rest
        .strip_suffix(".atom")
        .map(|n| (n, Format::Atom))
        .or_else(|| rest.strip_suffix(".rss").map(|n| (n, Format::Rss)))?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), format))
}

/// `GET /users/{username}.atom|.rss` — the local account's latest public
/// posts. Suspended/deleted accounts and remote names 404. A bare
/// `/users/{u}` redirects to the `/@{u}` web profile — Mastodon's
/// permalink shape.
async fn profile_feed(
    State(state): State<Arc<AppState>>,
    Path(rest): Path<String>,
) -> Result<Response> {
    let Some((username, format)) = split_feed_path(&rest) else {
        // `/@{u}` is the web profile: `/users/{u}` without a feed
        // extension is the same page — redirect (Mastodon's shape).
        // Only existing local users redirect; every other shape —
        // unknown users, bad or missing extensions — stays a JSON 404.
        // `rest` is percent-decoded — re-encode it for the Location.
        if bare_name(&rest)
            && account_mapper::load_local_by_username(&state.db, &rest)
                .await?
                .is_some()
        {
            let encoded: String = url::form_urlencoded::byte_serialize(rest.as_bytes()).collect();
            return Ok(
                Redirect::permanent(&format!("/@{}", encoded.replace('+', "%20"))).into_response(),
            );
        }
        return Err(Error::NotFound("feed".into()));
    };
    let account = account_mapper::load_local_by_username(&state.db, &username)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))?;
    let base = PostContext::new(&state).base_url();
    let mut feed = data::account_feed(
        &state.db,
        &state.config(),
        &base,
        &account,
        state.config().content.max_feed_items as i64,
    )
    .await?;
    feed.self_url = format!("{base}/users/{}.{}", account.username, format.ext());
    Ok(xml_response(format, &feed))
}

/// `GET /tags/{tag}.atom|.rss` — latest public posts carrying the tag
/// (case-insensitive). Unknown tags serve a valid empty feed. A bare
/// `/tags/{tag}` is the SPA hashtag page (browser accepts only — AP
/// fetchers keep the JSON 404: tag documents aren't served).
async fn tag_feed(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(rest): Path<String>,
) -> Result<Response> {
    let Some((tag, format)) = split_feed_path(&rest) else {
        return page_or_404(&state, &headers, &rest, "/tags");
    };
    let tag = tag.to_lowercase();
    let base = PostContext::new(&state).base_url();
    let mut feed = data::tag_feed(
        &state.db,
        &state.config(),
        &base,
        &tag,
        state.config().content.max_feed_items as i64,
    )
    .await?;
    feed.self_url = format!("{}.{}", data::tag_url(&base, &tag), format.ext());
    Ok(xml_response(format, &feed))
}

/// `GET /lists/{id}.atom|.rss` — the owner's list, including
/// `followers_only`/`private` member posts (never `direct`). Feed
/// readers authenticate with `Bearer` or `?access_token=`; `read:lists`
/// scope is required. Foreign lists 404. A bare `/lists/{id}` is the
/// SPA list page — checked before auth so browsers aren't 401'd.
async fn list_feed(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    headers: axum::http::HeaderMap,
    Path(rest): Path<String>,
) -> Result<Response> {
    let Some((id_str, format)) = split_feed_path(&rest) else {
        return page_or_404(&state, &headers, &rest, "/lists");
    };
    let auth = auth.0.ok_or(Error::Unauthorized)?;
    require_scope(&state.config(), &auth, "read:lists")?;
    let Ok(list_id) = id_str.parse::<i64>() else {
        return Err(Error::NotFound("feed".into()));
    };
    let list = social::lists::get_owned(&state.db, auth.account_id, list_id).await?;
    let base = PostContext::new(&state).base_url();
    let mut feed = data::list_feed(
        &state.db,
        &state.config(),
        &base,
        &list,
        state.config().content.max_feed_items as i64,
    )
    .await?;
    feed.self_url = format!("{base}/lists/{}.{}", list.id, format.ext());
    Ok(xml_response(format, &feed))
}

/// A single non-empty path segment with no `/` — plausibly a web page
/// name rather than a malformed feed path (which always carries an
/// extension or extra segments).
fn bare_name(rest: &str) -> bool {
    !rest.is_empty() && !rest.contains('/')
}

/// Extension-less feed paths are also web pages — `/tags/{tag}` and
/// `/lists/{id}` are SPA routes, so a bare name from a browser falls
/// through to the app shell (which also gets the feed-discovery
/// `<link>`s injected for tags). AP accepts and malformed feed shapes
/// — dotted names read as extension attempts, extra segments — stay
/// JSON 404s.
fn page_or_404(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    rest: &str,
    prefix: &str,
) -> Result<Response> {
    if bare_name(rest)
        && !rest.contains('.')
        && !crate::activitypub::permalinks::wants_document(headers)
    {
        // `rest` arrives percent-decoded — re-encode it as a single
        // path segment so non-ASCII names produce valid URLs.
        let encoded: String = url::form_urlencoded::byte_serialize(rest.as_bytes()).collect();
        return Ok(spa_entry(
            state,
            &format!("{prefix}/{}", encoded.replace('+', "%20")),
            frontend_choice(headers).as_deref(),
        ));
    }
    Err(Error::NotFound("feed".into()))
}

fn xml_response(format: Format, feed: &render::Feed) -> Response {
    let (body, content_type) = match format {
        Format::Atom => (
            render::render_atom(feed, Utc::now()),
            render::ATOM_CONTENT_TYPE,
        ),
        Format::Rss => (
            render::render_rss(feed, Utc::now()),
            render::RSS_CONTENT_TYPE,
        ),
    };
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// `<link rel="alternate">` tags for feed auto-discovery — Atom first,
/// then RSS. `base_path` is the extension-less feed path (e.g.
/// `/users/alice`); attribute values are XML-escaped. The frontend
/// epics embed the result into `<head>` when profile/hashtag pages
/// exist; list feeds are never advertised.
pub fn discovery_links(base_path: &str, title: &str) -> String {
    let title = escape(render::xml_safe(title));
    let base_path = escape(render::xml_safe(base_path));
    format!(
        "<link rel=\"alternate\" type=\"application/atom+xml\" title=\"{title}\" href=\"{base_path}.atom\" />\n\
         <link rel=\"alternate\" type=\"application/rss+xml\" title=\"{title}\" href=\"{base_path}.rss\" />"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_parses_extensions() {
        assert_eq!(
            split_feed_path("alice.atom"),
            Some(("alice".to_string(), Format::Atom))
        );
        assert_eq!(
            split_feed_path("rust.rss"),
            Some(("rust".to_string(), Format::Rss))
        );
    }

    #[test]
    fn split_rejects_non_feed_paths() {
        for rest in [
            "alice",
            "alice.json",
            "alice.atom.xml",
            "alice/statuses.atom",
            ".atom",
            "",
        ] {
            assert_eq!(split_feed_path(rest), None, "{rest} must not resolve");
        }
        // Dotted usernames with a feed suffix resolve fine.
        assert_eq!(
            split_feed_path("a.b.atom"),
            Some(("a.b".to_string(), Format::Atom))
        );
    }

    #[test]
    fn discovery_links_emit_both_tags() {
        let html = discovery_links("/users/alice", "Alice's feed");
        assert!(html.contains(
            "<link rel=\"alternate\" type=\"application/atom+xml\" title=\"Alice&apos;s feed\" href=\"/users/alice.atom\" />"
        ));
        assert!(html.contains("type=\"application/rss+xml\""));
        assert!(html.contains("href=\"/users/alice.rss\""));
    }

    #[test]
    fn discovery_links_escape_attributes() {
        let html = discovery_links("/users/a\"<b>", "t<\"&");
        // The hostile input survives only in escaped form.
        for needle in ["&quot;", "&lt;", "&amp;"] {
            assert!(html.contains(needle), "missing {needle}");
        }
        assert!(!html.contains("a\"<b>"));
        assert!(!html.contains("t<\"&"));
    }

    #[test]
    fn discovery_links_drop_xml_invalid_characters() {
        let html = discovery_links("/users/a\u{1}", "t\u{0}");
        assert!(html.contains("href=\"/users/a.atom\""));
        assert!(!html.contains('\u{0}') && !html.contains('\u{1}'));
    }
}
