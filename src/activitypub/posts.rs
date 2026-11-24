//! Status objects on the wire — Note/Article/Question building,
//! Create/Update/Delete envelopes, and the `/ap/users/{u}/statuses/{id}`
//! object GET (epic 12). `Article` objects implement FEP-b2b8
//! long-form text: `name` headline, HTML `summary`, `image` cover and
//! a `preview` Note.
//!
//! Pubby builds the base `Object` and the activity envelopes; Yonder
//! injects what `Object` can't express — `source` (markdown
//! round-trip), poll fields (`oneOf`/`anyOf`/`endTime`) — into the
//! emitted object JSON (the epic-09 banner-injection pattern).
//!
//! **Object serving is visibility-scoped, not just signed-fetch**:
//! `authorized_fetch` gates `public`/`unlisted`; `followers_only`
//! requires the verified signer to be the author or an accepted
//! follower; `direct` requires the author or a mentioned recipient —
//! resolved via `can_see`. `local` statuses take the private path:
//! the signer must resolve to an account *local to this instance*.
//! Only public/unlisted documents enter the shared `doc_cache`;
//! private documents render per-request.
//!
//! **`local` never federates**: [`publish_create`] and
//! [`publish_update`] return early for it — no `activities` row, no
//! deliveries, no outbox entry — so instance-only content can't leak
//! through any publish call site (create, edit, scheduled publish,
//! media/admin re-federation, quote authorization).

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pubby_sdk::Object;

use crate::http::AppState;
use crate::posts::{self, mentions::ResolvedMention, render, PostContext, StatusRow};
use crate::{Error, Result};

use super::{actors::sync_call, context, delivery, ApContext};

/// The doc-cache key for a served status object.
fn object_key(status_id: i64) -> pubby_sdk::cache::CacheKey {
    pubby_sdk::cache::route_key(&["status", &status_id.to_string()])
}

/// Drop the cached object document (edit/delete/tombstone).
pub fn invalidate_object(ap: &ApContext, status_id: i64) {
    ap.core.doc_cache.invalidate(&[object_key(status_id)]);
}

/// Load the author's `actor_id` and the `username`/`followers` bits
/// object building needs.
async fn author_parts(ctx: &PostContext, account_id: i64) -> Result<(String, String)> {
    let row =
        sqlx::query("SELECT username, actor_id FROM accounts WHERE id = $1 AND deleted_at IS NULL")
            .bind(account_id)
            .fetch_optional(&ctx.db)
            .await?
            .ok_or_else(|| Error::NotFound("account".into()))?;
    Ok((row.get("username"), row.get("actor_id")))
}

/// The `status_mentions` actor URLs (audience + Mention tags).
async fn mention_audience(ctx: &PostContext, status_id: i64) -> Result<Vec<ResolvedMention>> {
    let rows = sqlx::query(
        "SELECT a.id, a.actor_id, a.url, a.username, a.domain \
         FROM status_mentions sm JOIN accounts a ON a.id = sm.account_id \
         WHERE sm.status_id = $1 ORDER BY a.id",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let username: String = r.get("username");
            let domain: Option<String> = r.get("domain");
            let actor_url: String = r.get("actor_id");
            ResolvedMention {
                account_id: r.get("id"),
                actor_url: actor_url.clone(),
                profile_url: r
                    .get::<Option<String>, _>("url")
                    .filter(|u| !u.is_empty())
                    .unwrap_or(actor_url),
                acct: match &domain {
                    Some(d) => format!("@{username}@{d}"),
                    None => format!("@{username}"),
                },
                token: String::new(),
            }
        })
        .collect())
}

/// Best-effort `mediaType` for a Markdown image URL — extension
/// sniffing only; unknown types stay `image/*` so attachment consumers
/// still classify it as an image.
fn inline_image_media_type(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "image/*",
    }
}

/// `attachment` Documents for the status's media rows, plus the
/// Markdown `![alt](url)` images as standard `Image` attachments —
/// clients without inline-image support (Mastodon, Pleroma, Akkoma…)
/// render those instead of the `<img>` in `content`.
async fn attachments(ctx: &PostContext, status: &StatusRow) -> Result<Vec<Value>> {
    let base = ctx.base_url();
    let media = posts::entity::media_for_status(ctx, status.id).await?;
    let mut docs: Vec<Value> = media
        .iter()
        .filter_map(|m| {
            let url = posts::entity::media_url(&ctx.config, &base, m)?;
            let doc_type = match m.kind.as_str() {
                "image" => "Image",
                "video" => "Video",
                "audio" => "Audio",
                _ => "Document",
            };
            Some(json!({
                "type": doc_type,
                "url": url,
                "mediaType": m.content_type,
                "name": m.description,
                "blurhash": m.blurhash,
                "width": m.width,
                "height": m.height,
            }))
        })
        .collect();

    if status.content_type == "text/markdown" {
        let mut seen: std::collections::HashSet<String> = docs
            .iter()
            .filter_map(|d| d.get("url").and_then(|u| u.as_str()).map(str::to_string))
            .collect();
        for image in render::markdown_images(&status.content, &ctx.config) {
            if !seen.insert(image.url.clone()) {
                continue;
            }
            let mut doc = json!({
                "type": "Image",
                "url": image.url,
                "mediaType": inline_image_media_type(&image.url),
            });
            if !image.alt.is_empty() {
                doc["name"] = json!(image.alt);
            }
            docs.push(doc);
        }
    }
    Ok(docs)
}

/// Poll `oneOf`/`anyOf` options + `endTime` — injected into the
/// emitted Question object.
async fn poll_fields(ctx: &PostContext, poll_id: i64) -> Result<Value> {
    let poll = sqlx::query("SELECT expires_at, multiple FROM polls WHERE id = $1")
        .bind(poll_id)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or_else(|| Error::NotFound("poll".into()))?;
    let options =
        sqlx::query("SELECT title, votes_count FROM poll_options WHERE poll_id = $1 ORDER BY idx")
            .bind(poll_id)
            .fetch_all(&ctx.db)
            .await?;
    let option_docs: Vec<Value> = options
        .iter()
        .map(|o| {
            json!({
                "type": "Note",
                "name": o.get::<String, _>("title"),
                "replies": {
                    "type": "Collection",
                    "totalItems": o.get::<i32, _>("votes_count"),
                },
            })
        })
        .collect();
    let mut fields = json!({});
    let key = if poll.get::<bool, _>("multiple") {
        "anyOf"
    } else {
        "oneOf"
    };
    fields[key] = Value::Array(option_docs);
    if let Some(end) = poll.get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at") {
        fields["endTime"] = json!(pubby_sdk::model::dt_to_iso(&end));
    }
    Ok(fields)
}

/// The status's representative image as an AS2 `Image` — the
/// `cover_media_id` pick when set, else the first image attachment
/// (FEP-b2b8 `image`). `None` when the status has no usable image.
async fn article_image(ctx: &PostContext, status: &StatusRow) -> Result<Option<Value>> {
    let base = ctx.base_url();
    let media = posts::entity::media_for_status(ctx, status.id).await?;
    let chosen = status
        .cover_media_id
        .and_then(|id| media.iter().find(|m| m.id == id))
        .or_else(|| media.iter().find(|m| m.kind == "image"));
    let Some(m) = chosen else { return Ok(None) };
    let Some(url) = posts::entity::media_url(&ctx.config, &base, m) else {
        return Ok(None);
    };
    let mut doc = json!({
        "type": "Image",
        "url": url,
        "mediaType": m.content_type,
    });
    if let Some(description) = &m.description {
        doc["name"] = json!(description);
    }
    if let Some(width) = m.width {
        doc["width"] = json!(width);
    }
    if let Some(height) = m.height {
        doc["height"] = json!(height);
    }
    Ok(Some(doc))
}

/// The FEP-b2b8 `summary`/`preview` body: the content warning and the
/// article lede, each as a `<p>` of escaped plain text. `None` when
/// the article has neither.
fn article_summary_html(status: &StatusRow) -> Option<String> {
    let mut paragraphs = Vec::new();
    if let Some(cw) = status.spoiler_text.as_deref().filter(|s| !s.is_empty()) {
        paragraphs.push(format!("<p>{}</p>", pubby_sdk::content::html_escape(cw)));
    }
    if let Some(lede) = status.summary.as_deref().filter(|s| !s.is_empty()) {
        paragraphs.push(format!("<p>{}</p>", pubby_sdk::content::html_escape(lede)));
    }
    (!paragraphs.is_empty()).then(|| paragraphs.join(""))
}

/// The full object document (Note/Article/Question) for a stored
/// status — served verbatim by the object GET and embedded in
/// Create/Update.
pub async fn build_object(ctx: &PostContext, status: &StatusRow) -> Result<Value> {
    let (_username, actor_id) = author_parts(ctx, status.account_id).await?;
    let followers = format!("{actor_id}/followers");
    let mentions = mention_audience(ctx, status.id).await?;
    let mention_urls = posts::mentions::mention_actor_urls(&mentions);
    let (to, cc) = posts::addressing(&status.visibility, &followers, &mention_urls);

    let mut object = Object::new(status.uri.clone().unwrap_or_default());
    let article = posts::is_article(status);
    let event = event_bundle(ctx, status).await?;
    object.object_type = if status.poll_id.is_some() {
        "Question"
    } else if article {
        "Article"
    } else if event.is_some() {
        "Event"
    } else {
        "Note"
    }
    .to_string();
    object.content = status
        .content_html
        .clone()
        .unwrap_or_else(|| pubby_sdk::content::html_escape(&status.content));
    object.url = json!(status.url.clone().unwrap_or_default());
    object.attributed_to = json!(actor_id);
    object.in_reply_to = match status.in_reply_to_id {
        Some(id) => {
            sqlx::query_scalar::<_, Option<String>>("SELECT uri FROM statuses WHERE id = $1")
                .bind(id)
                .fetch_optional(&ctx.db)
                .await?
                .flatten()
        }
        None => None,
    };
    object.published = Some(status.created_at);
    object.updated = status.edited_at.or(Some(status.updated_at));
    // Articles carry the spoiler + lede as HTML `summary` (injected
    // below); notes keep the plain-text content warning.
    object.summary = if article {
        None
    } else {
        status.spoiler_text.clone()
    };
    object.sensitive = status.sensitive;
    object.tag = posts::mentions::mention_tags(&mentions);
    object.tag.extend(pubby_sdk::build_hashtag_tags(
        hashtags(ctx, status.id).await?,
        |name| format!("{}/tags/{name}", ctx.base_url()),
    ));
    // Event media loads once — attachments and the flyer `image`
    // share the same list.
    let event_media = if event.is_some() {
        Some(posts::entity::media_for_status(ctx, status.id).await?)
    } else {
        None
    };
    object.attachment = match (&event, &event_media) {
        (Some(bundle), Some(media)) => event_attachments(ctx, status, bundle, media)?,
        _ => attachments(ctx, status).await?,
    };
    object.to = to;
    object.cc = cc;
    object.language = status.language.clone();

    let mut doc = object.to_json();
    // `source` (markdown round-trip) — `Object` has no field for it.
    doc["source"] = json!({
        "content": status.content,
        "mediaType": status.content_type,
    });
    // Catodon-style event invite (migration 0063): a `direct` status
    // marked in `status_event_invites` carries the event URI under
    // both property names — `yonder:` for Yonder peers, `catodon:` so
    // Catodon renders "invited you to an event".
    if status.visibility == "direct" {
        let invite_uri: Option<String> =
            sqlx::query_scalar("SELECT event_uri FROM status_event_invites WHERE status_id = $1")
                .bind(status.id)
                .fetch_optional(&ctx.db)
                .await?;
        if let Some(uri) = invite_uri {
            doc["yonder:inviteTo"] = json!(uri);
            doc["catodon:inviteTo"] = json!(uri);
        }
    }
    // Epic 20 — Emoji tags for every local `:code:` in the text, so
    // remote instances can render and record them (pubby has no
    // Emoji tag builder; `doc["tag"]` is the emitted array). A map
    // load failure degrades to no tags, never a failed status build.
    let emoji_map =
        crate::emoji::render::EmojiMap::load_local(&ctx.db, &ctx.config, &ctx.base_url())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "emoji map load failed; no Emoji tags emitted");
                crate::emoji::render::EmojiMap::empty()
            });
    let haystack = format!(
        "{} {}",
        status.content,
        status.spoiler_text.clone().unwrap_or_default()
    );
    if !emoji_map.is_empty() {
        if !doc["tag"].is_array() {
            doc["tag"] = json!([]);
        }
        if let Some(tags) = doc["tag"].as_array_mut() {
            let base = ctx.base_url();
            for meta in emoji_map.used_in(&haystack) {
                tags.push(meta.ap_tag(&base));
            }
        }
    }
    if let Some(poll_id) = status.poll_id {
        for (k, v) in poll_fields(ctx, poll_id)
            .await?
            .as_object()
            .cloned()
            .unwrap_or_default()
        {
            doc[k] = v;
        }
    }
    // FEP-b2b8 Article properties `Object` can't express: `name`
    // headline, HTML `summary`, `image` cover and the `preview` Note.
    if article {
        if let Some(title) = status.title.as_deref().filter(|t| !t.is_empty()) {
            doc["name"] = json!(title);
            if let Some(lang) = status.language.as_deref() {
                doc["nameMap"] = json!({ lang: title });
            }
        }
        if let Some(summary_html) = article_summary_html(status) {
            doc["summary"] = json!(summary_html);
            if let Some(lang) = status.language.as_deref() {
                doc["summaryMap"] = json!({ lang: summary_html });
            }
        }
        let image = article_image(ctx, status).await?;
        if let Some(img) = &image {
            doc["image"] = img.clone();
        }
        // The `preview` Note repeats the metadata consumers need
        // without fetching the full article (FEP-b2b8).
        let mut preview = json!({
            "type": "Note",
            "attributedTo": actor_id.clone(),
            "published": pubby_sdk::model::dt_to_iso(&status.created_at),
            "url": status.url.clone(),
        });
        if let Some(title) = status.title.as_deref().filter(|t| !t.is_empty()) {
            preview["name"] = json!(title);
        }
        if let Some(summary_html) = article_summary_html(status) {
            preview["content"] = json!(summary_html);
        }
        if let Some(updated) = status.edited_at.or(Some(status.updated_at)) {
            preview["updated"] = json!(pubby_sdk::model::dt_to_iso(&updated));
        }
        if let Some(tags) = doc.get("tag").cloned() {
            preview["tag"] = tags;
        }
        if let Some(img) = image {
            preview["attachment"] = json!([img]);
        }
        doc["preview"] = preview;
        doc["generator"] = json!({
            "type": "Application",
            "name": "Yonder",
        });
    }
    // FEP-8a8e Event fields `Object` can't express — name, generated
    // summary, times, locations, join mode, capacity, collections.
    if let Some(bundle) = &event {
        for (k, v) in event_fields(
            ctx,
            status,
            bundle,
            &actor_id,
            event_media.as_deref().unwrap_or(&[]),
        )?
        .as_object()
        .cloned()
        .unwrap_or_default()
        {
            doc[k] = v;
        }
        doc["generator"] = json!({
            "type": "Application",
            "name": "Yonder",
        });
    }
    pubby_sdk::quotes::allow_public_quotes(&mut doc);
    // Quotes (epic 13): stamp every FEP-0449 spelling. `quote_of_id`
    // resolves through the target row; `quote_uri` covers remote
    // targets not materialized yet.
    if let Some(uri) = posts::quote_target_uri(&ctx.db, status).await? {
        pubby_sdk::quotes::set_quote_target(&mut doc, &uri);
    }
    // FEP-044f approval — Mastodon leaves the quote pending until this
    // stamp verifies against the quoted author's QuoteAuthorization.
    if let Some(uri) = &status.quote_authorization_uri {
        doc["quoteAuthorization"] = json!(uri);
    }
    Ok(doc)
}

/// The status's stored hashtag names.
async fn hashtags(ctx: &PostContext, status_id: i64) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
         WHERE st.status_id = $1 ORDER BY t.name",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await
    .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Event (FEP-8a8e) object fields — plan.md §5.2
// ---------------------------------------------------------------------------

/// The `events` bundle for an event status — `None` for non-events
/// and (defensively) event rows missing their side-table row.
async fn event_bundle(
    ctx: &PostContext,
    status: &StatusRow,
) -> Result<Option<crate::events::entity::EventBundle>> {
    if !posts::is_event(status) {
        return Ok(None);
    }
    let mut bundles = crate::events::entity::load_many(&ctx.db, &[status.id], None).await?;
    Ok(bundles.remove(&status.id))
}

/// One AS2 `Document` for a media row — shared by the generic
/// attachment list and the event variant (which adds `focalPoint`).
fn media_doc(
    config: &crate::config::Config,
    base_url: &str,
    m: &posts::entity::MediaRow,
) -> Option<Value> {
    let url = posts::entity::media_url(config, base_url, m)?;
    let doc_type = match m.kind.as_str() {
        "image" => "Image",
        "video" => "Video",
        "audio" => "Audio",
        _ => "Document",
    };
    let mut doc = json!({
        "type": doc_type,
        "url": url,
        "mediaType": m.content_type,
        "name": m.description,
        "blurhash": m.blurhash,
        "width": m.width,
        "height": m.height,
    });
    if let (Some(x), Some(y)) = (m.focus_x, m.focus_y) {
        doc["focalPoint"] = json!([x, y]);
    }
    Some(doc)
}

/// Event `attachment[]`: the banner first with `isBannerImage`, then
/// the remaining media (the flyer is promoted to `image`, so it is
/// not repeated), then Markdown inline images like notes get.
fn event_attachments(
    ctx: &PostContext,
    status: &StatusRow,
    bundle: &crate::events::entity::EventBundle,
    media: &[posts::entity::MediaRow],
) -> Result<Vec<Value>> {
    let base = ctx.base_url();
    let banner_id = bundle.row.banner_media_id;
    let flyer_id = bundle.row.flyer_media_id;
    let mut banner_doc = None;
    let mut docs: Vec<Value> = Vec::new();
    for m in media {
        if Some(m.id) == flyer_id {
            continue;
        }
        let Some(mut doc) = media_doc(&ctx.config, &base, m) else {
            continue;
        };
        if Some(m.id) == banner_id {
            doc["isBannerImage"] = json!(true);
            banner_doc = Some(doc);
        } else {
            docs.push(doc);
        }
    }
    // Markdown `![alt](url)` images — same treatment as notes.
    if status.content_type == "text/markdown" {
        let mut seen: std::collections::HashSet<String> = docs
            .iter()
            .chain(banner_doc.iter())
            .filter_map(|d| d.get("url").and_then(|u| u.as_str()).map(str::to_string))
            .collect();
        for image in render::markdown_images(&status.content, &ctx.config) {
            if !seen.insert(image.url.clone()) {
                continue;
            }
            let mut doc = json!({
                "type": "Image",
                "url": image.url,
                "mediaType": inline_image_media_type(&image.url),
            });
            if !image.alt.is_empty() {
                doc["name"] = json!(image.alt);
            }
            docs.push(doc);
        }
    }
    // `Link` attachments for clients that can't render events
    // natively: the permalink ("copy link") and the ICS feed
    // ("download calendar"). `mediaType` lets aware parsers skip the
    // calendar link when promoting Links to virtual locations.
    if let Some(page) = status.url.as_deref().or(status.uri.as_deref()) {
        docs.push(json!({
            "type": "Link",
            "href": page,
            "mediaType": "text/html",
            "name": "Event page",
        }));
    }
    docs.push(json!({
        "type": "Link",
        "href": format!("{}/api/v1/yonder/events/{}/ics", base.trim_end_matches('/'), status.id),
        "mediaType": "text/calendar",
        "name": "Calendar (.ics)",
    }));
    let mut out = Vec::with_capacity(docs.len() + 1);
    if let Some(banner) = banner_doc {
        out.push(banner);
    }
    out.extend(docs);
    Ok(out)
}

/// One `event_locations` row → the wire `location[]` entry —
/// `Place` with `PostalAddress`, or `VirtualLocation`.
fn event_location_doc(loc: &crate::events::model::EventLocationRow) -> Value {
    use crate::events::model::LocationKind;
    let mut doc = json!({
        "type": if loc.kind == LocationKind::Place { "Place" } else { "VirtualLocation" },
    });
    if let Some(name) = &loc.name {
        doc["name"] = json!(name);
    }
    match loc.kind {
        LocationKind::Place => {
            let has_structured = loc.street_address.is_some()
                || loc.locality.is_some()
                || loc.region.is_some()
                || loc.postal_code.is_some()
                || loc.country.is_some();
            if has_structured {
                let mut address = json!({ "type": "PostalAddress" });
                for (key, value) in [
                    ("streetAddress", &loc.street_address),
                    ("addressLocality", &loc.locality),
                    ("addressRegion", &loc.region),
                    ("postalCode", &loc.postal_code),
                    ("addressCountry", &loc.country),
                ] {
                    if let Some(v) = value {
                        address[key] = json!(v);
                    }
                }
                doc["address"] = address;
            } else if let Some(text) = &loc.address_text {
                doc["address"] = json!(text);
            }
            if let Some(lat) = loc.latitude {
                doc["latitude"] = json!(lat);
            }
            if let Some(lon) = loc.longitude {
                doc["longitude"] = json!(lon);
            }
        }
        LocationKind::Virtual => {
            if let Some(url) = &loc.url {
                doc["url"] = json!(url);
            }
        }
    }
    doc
}

/// The FEP-8a8e fields `build_object` merges onto an Event document
/// (plan.md §5.2). `actor_id` is the status author's actor URL — the
/// `requiredJoinVisibility`/organizer fallback.
fn event_fields(
    ctx: &PostContext,
    status: &StatusRow,
    bundle: &crate::events::entity::EventBundle,
    actor_id: &str,
    media: &[posts::entity::MediaRow],
) -> Result<Value> {
    use crate::events::model::{JoinMode, LocationKind};
    let row = &bundle.row;
    let mut doc = json!({});

    if let Some(title) = status.title.as_deref().filter(|t| !t.is_empty()) {
        doc["name"] = json!(title);
        if let Some(lang) = status.language.as_deref() {
            doc["nameMap"] = json!({ lang: title });
        }
    }

    // Generated summary: preamble + plain-text excerpt.
    let summary = crate::events::summary::ap_summary_html(
        row,
        &bundle.locations,
        status.language.as_deref(),
        Some(status.content.as_str()),
        ctx.config.events.summary_excerpt_chars,
    );
    if !summary.is_empty() {
        doc["summary"] = json!(summary);
        if let Some(lang) = status.language.as_deref() {
            doc["summaryMap"] = json!({ lang: doc["summary"].clone() });
        }
    }

    // Times in the event zone (Gancio style). Postponed events omit
    // start/end entirely — `previousStartTime` carries the old slot.
    if let Some(start) = row.start_time {
        doc["startTime"] = json!(crate::events::summary::rfc3339_in_zone(row, start));
        if let Some(end) = row.end_time {
            doc["endTime"] = json!(crate::events::summary::rfc3339_in_zone(row, end));
        }
        doc["displayEndTime"] = json!(row.display_end_time);
    }
    if let Some(tz) = &row.timezone {
        doc["timezone"] = json!(tz);
    }
    doc["eventStatus"] = json!(super::context::event_status_token(row.status));
    doc["status"] = json!(super::context::ical_status(row.status));
    if let Some(prev) = row.previous_start_time {
        doc["previousStartTime"] = json!(crate::events::summary::rfc3339_in_zone(row, prev));
    }

    let locations: Vec<Value> = bundle.locations.iter().map(event_location_doc).collect();
    if !locations.is_empty() {
        doc["location"] = json!(locations);
        let all_virtual = bundle
            .locations
            .iter()
            .all(|l| l.kind == LocationKind::Virtual);
        doc["isOnline"] = json!(all_virtual);
    }

    match row.categories.as_slice() {
        [] => {}
        [single] => doc["category"] = json!(single),
        many => doc["category"] = json!(many),
    }

    doc["joinMode"] = json!(row.join_mode.to_string());
    if let Some(url) = &row.external_participation_url {
        doc["externalParticipationUrl"] = json!(url);
    }
    // Approval-gated join modes name the organizer as the required
    // join visibility (plan.md §5.2).
    if matches!(row.join_mode, JoinMode::Restricted | JoinMode::Invite) {
        doc["requiredJoinVisibility"] = json!([actor_id]);
    }
    if row.is_capacity_limited() {
        if let Some(cap) = row.maximum_attendee_capacity {
            doc["maximumAttendeeCapacity"] = json!(cap);
        }
        if let Some(remaining) = row.remaining_capacity() {
            doc["remainingAttendeeCapacity"] = json!(remaining);
        }
    }

    // Collection stubs — the OrderedCollection/OrganizersCollection
    // endpoints land with the RSVP section; emit id + totalItems now.
    let object_id = status.uri.clone().unwrap_or_default();
    doc["attendees"] = json!({
        "id": format!("{object_id}/attendees"),
        "type": "OrderedCollection",
        "totalItems": row.participants_count,
    });
    let organizer_items: Vec<String> = bundle
        .organizers
        .iter()
        .filter_map(|o| {
            o.account_id
                .and_then(|aid| bundle.organizer_accounts.get(&aid))
        })
        .map(|a| a.actor_id.clone())
        .collect();
    let organizer_items = if organizer_items.is_empty() {
        vec![actor_id.to_string()]
    } else {
        organizer_items
    };
    doc["organizers"] = json!({
        "id": format!("{object_id}/organizers"),
        "type": "OrganizersCollection",
        "totalItems": organizer_items.len(),
        "items": organizer_items,
    });

    // Flyer → `image` (never duplicating the banner).
    if let Some(flyer_id) = row.flyer_media_id {
        if let Some(m) = media.iter().find(|m| m.id == flyer_id) {
            if let Some(url) = posts::entity::media_url(&ctx.config, &ctx.base_url(), m) {
                let mut image = json!({
                    "type": "Image",
                    "url": url,
                    "mediaType": m.content_type,
                    "name": m.description,
                });
                if let (Some(w), Some(h)) = (m.width, m.height) {
                    image["width"] = json!(w);
                    image["height"] = json!(h);
                }
                doc["image"] = image;
            }
        }
    }
    Ok(doc)
}

/// `Object` → the pubby `Object` struct the activity builders want —
/// rebuilt from our emitted doc (addressing + the model fields).
fn object_for_activity(doc: &Value) -> Object {
    Object::from_json(doc)
}

/// Create activity → `delivery::publish` (stores the `activities` row,
/// invalidates doc-cache prefixes, fans out through the durable
/// ledger).
pub async fn publish_create(
    ap: &ApContext,
    ctx: &PostContext,
    status: &StatusRow,
    _resolved: &[ResolvedMention],
) -> Result<()> {
    if status.visibility == "local" {
        return Ok(());
    }
    let username = username_of(ctx, status.account_id).await?;
    let doc = build_object(ctx, status).await?;
    let obj = object_for_activity(&doc);
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let mut activity = sync_call(move || handler.outbox.build_create_activity(&obj)).await;
    // Inject the fields `Object::to_json` can't carry.
    if let Some(object) = activity.get_mut("object") {
        merge_object_extras(object, &doc);
    }
    if posts::is_event(status) {
        activity["@context"] = context::event_context();
    }
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// `PUT` edit → `Update` activity carrying the full new object.
pub async fn publish_update(
    ap: &ApContext,
    ctx: &PostContext,
    status: &StatusRow,
    _resolved: &[ResolvedMention],
) -> Result<()> {
    if status.visibility == "local" {
        return Ok(());
    }
    let username = username_of(ctx, status.account_id).await?;
    let doc = build_object(ctx, status).await?;
    let obj = object_for_activity(&doc);
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let mut activity = sync_call(move || handler.outbox.build_update_activity_obj(&obj)).await;
    if let Some(object) = activity.get_mut("object") {
        merge_object_extras(object, &doc);
    }
    if posts::is_event(status) {
        activity["@context"] = context::event_context();
        // FEP-8a8e §7: an event Update reaches the usual audience
        // *plus* every remote participant's actor — participants who
        // don't follow the organizer still learn about reschedules,
        // cancellations and moved-online transitions. The embedded
        // object keeps the normal audience; only the activity-level
        // `cc` widens (pubby collects recipient inboxes from there).
        let extra = event_participant_actors(ctx, status.id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(status_id = status.id, error = %e,
                    "event participant audience lookup failed");
                Vec::new()
            });
        if !extra.is_empty() {
            let cc = activity.get_mut("cc").and_then(|v| v.as_array_mut());
            match cc {
                Some(arr) => {
                    for actor in extra {
                        let v = json!(actor);
                        if !arr.contains(&v) {
                            arr.push(v);
                        }
                    }
                }
                None => activity["cc"] = json!(extra),
            }
        }
    }
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// Remote participant actor URLs for an event — states that still
/// care about updates (`accepted`, `tentative`, `pending`, `invited`;
/// declined/rejected/ignored rows opted out). Local participants get
/// stream updates already; remote rows carry `actor_id` URLs.
async fn event_participant_actors(ctx: &PostContext, status_id: i64) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        "SELECT a.actor_id FROM event_participations p \
         JOIN accounts a ON a.id = p.account_id \
         WHERE p.status_id = $1 AND a.domain IS NOT NULL \
           AND p.state IN ('accepted','tentative','pending','invited')",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await
    .map_err(Into::into)
}

/// Delete → `Delete(object_uri)` to the stored audience's inboxes.
pub async fn publish_delete(ap: &ApContext, ctx: &PostContext, status: &StatusRow) -> Result<()> {
    let username = username_of(ctx, status.account_id).await?;
    let Some(uri) = status.uri.clone() else {
        return Ok(());
    };
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let activity = sync_call(move || handler.outbox.build_delete_activity_obj(&uri)).await;
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// Copy the doc-only fields (`source`, poll fields, quote fields,
/// `interactionPolicy`) `Object::to_json` dropped onto the activity's
/// embedded object.
pub(crate) fn merge_object_extras(target: &mut Value, doc: &Value) {
    for key in [
        "source",
        "oneOf",
        "anyOf",
        "endTime",
        "votersCount",
        "quote",
        "quoteUri",
        "quoteUrl",
        "_misskey_quote",
        "quoteAuthorization",
        "interactionPolicy",
        // Article fields `Object` doesn't model (FEP-b2b8).
        "name",
        "nameMap",
        "summary",
        "summaryMap",
        "image",
        "preview",
        "generator",
        // FEP-8a8e Event fields (plan.md §5.2) — an object is a
        // Question or an Event, never both, so sharing `endTime`
        // with polls is safe.
        "startTime",
        "endTime",
        "displayEndTime",
        "timezone",
        "eventStatus",
        "status",
        "previousStartTime",
        "location",
        "isOnline",
        "category",
        "joinMode",
        "externalParticipationUrl",
        "requiredJoinVisibility",
        "maximumAttendeeCapacity",
        "remainingAttendeeCapacity",
        "attendees",
        "organizers",
    ] {
        if let Some(v) = doc.get(key) {
            target[key] = v.clone();
        }
    }
}

async fn username_of(ctx: &PostContext, account_id: i64) -> Result<String> {
    sqlx::query_scalar::<_, String>("SELECT username FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))
}

// ---------------------------------------------------------------------------
// Object GET — `/ap/users/{username}/statuses/{id}`
// ---------------------------------------------------------------------------

/// Resolve the signer's actor URL to an `accounts.id` — `None` for
/// unknown actors (a foreign signer is never a follower/recipient
/// here). `pub(crate)` — the featured collection applies the same
/// block gate.
pub(crate) async fn signer_account_id(ctx: &PostContext, actor_url: &str) -> Result<Option<i64>> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(actor_url)
        .fetch_optional(&ctx.db)
        .await
        .map_err(Into::into)
}

/// `GET /ap/users/{username}/statuses/{id}` — visibility-scoped object
/// serving. `public`/`unlisted` follow `authorized_fetch` and use the
/// shared doc-cache; `followers_only`/`direct` resolve the verified
/// signer and apply `can_see` — unauthorized gets a 404 (never a
/// leak), and private docs are never cached.
pub async fn serve_object(
    State(state): State<Arc<AppState>>,
    Path((username, status_id)): Path<(String, i64)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    serve_named_object(&state, &username, status_id, &headers, &uri).await
}

/// [`serve_object`] with the author's username already resolved —
/// web-permalink callers (`permalinks::status`) treat the path's acct
/// as decorative.
pub(crate) async fn serve_named_object(
    state: &Arc<AppState>,
    username: &str,
    status_id: i64,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Response {
    match serve_object_inner(state, username, status_id, headers, uri).await {
        Ok((doc, public)) => {
            super::edge::apply(json_response(doc), public, super::edge::anonymous(headers))
        }
        Err(Error::Unauthorized) => StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => super::edge::apply(
            StatusCode::NOT_FOUND.into_response(),
            true,
            super::edge::anonymous(headers),
        ),
    }
}

/// Gate + build the object document. The flag is whether the document
/// is anonymous-readable (`public`/`unlisted`) — signer-gated
/// documents are never edge-cacheable.
async fn serve_object_inner(
    state: &Arc<AppState>,
    username: &str,
    status_id: i64,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(Value, bool)> {
    let Some(ap) = &state.ap else {
        return Err(Error::NotFound("ap".into()));
    };
    let ctx = PostContext::new(state);
    let status = posts::load_any(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if !status.local || status.tombstone {
        return Err(Error::NotFound("status".into()));
    }

    // Signature check: under `authorized_fetch` every request must be
    // signed; private visibilities always need a verified signer.
    let signer = verify_signature(state, ap, username, headers, uri).await;
    let public_doc = matches!(status.visibility.as_str(), "public" | "unlisted");
    if !public_doc && signer.is_none() {
        // 404, never 401 — don't reveal the object exists.
        return Err(Error::NotFound("status".into()));
    }
    if public_doc && state.config().federation.authorized_fetch && signer.is_none() {
        return Err(Error::Unauthorized);
    }

    // A verified signer the author blocked — or who blocked/domain-
    // blocked the author — gets the same 404 private docs do. Only
    // applies when the signature resolves to a known local account
    // row; unknown remote signers can't be in `blocks`.
    let signer_account = match &signer {
        Some(actor) => signer_account_id(&ctx, actor).await?,
        None => None,
    };
    if let Some(sid) = signer_account {
        if crate::social::visibility::author_hidden(&ctx.db, sid, status.account_id).await? {
            return Err(Error::NotFound("status".into()));
        }
    }

    if public_doc {
        // Shared doc-cache — public/unlisted only.
        let key = object_key(status_id);
        let ctx2 = ctx.clone();
        let cached = ap
            .core
            .doc_cache
            .get_or_render(
                key,
                move || {
                    let ctx = ctx2.clone();
                    super::actors::sync_block(async move {
                        let status = posts::load_any(&ctx.db, status_id)
                            .await
                            .map_err(|e| pubby_sdk::ActivityPubError::General(e.to_string()))?;
                        match status {
                            Some(s) if s.local && !s.tombstone => build_object(&ctx, &s)
                                .await
                                .map(Some)
                                .map_err(|e| pubby_sdk::ActivityPubError::General(e.to_string())),
                            _ => Ok(None),
                        }
                    })
                },
                None,
                None,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
        return cached
            .map(|doc| (doc, true))
            .ok_or_else(|| Error::NotFound("status".into()));
    }

    // followers_only / direct — the verified signer must be allowed.
    if signer.is_none() {
        return Err(Error::NotFound("status".into()));
    }
    let mut allowed = match signer_account {
        Some(id) => posts::can_see(&ctx.db, &status, Some(id)).await?,
        None => false,
    };
    // `direct` objects are fetched by the *remote instance actor* on
    // behalf of the addressed user — that actor usually has no
    // `accounts` row (and never a mention), so `can_see` alone 404s
    // every invite fetch (§16 invitees-only events depend on it).
    // The signature already verified the keyId URL; a signer domain
    // matching a mentioned account's domain is the delegation proof.
    if !allowed && status.visibility == "direct" {
        if let Some(domain) = signer.as_deref().and_then(super::moderation::actor_domain) {
            allowed = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM status_mentions sm \
                 JOIN accounts a ON a.id = sm.account_id \
                 WHERE sm.status_id = $1 AND lower(a.domain) = $2)",
            )
            .bind(status.id)
            .bind(&domain)
            .fetch_one(&ctx.db)
            .await?;
        }
    }
    if allowed {
        build_object(&ctx, &status).await.map(|doc| (doc, false))
    } else {
        Err(Error::NotFound("status".into()))
    }
}

/// How long a failed signature-time actor lookup suppresses retries.
/// Pubby caches only *successful* actor fetches, so without a negative
/// marker every signed request from an unreachable actor pays the full
/// remote-fetch timeout again — the `/ap/` tail latency. Short enough
/// that a recovered origin verifies again quickly.
const SIG_FETCH_NEGATIVE_TTL: Duration = Duration::from_secs(300);

/// Cache key for [`SIG_FETCH_NEGATIVE_TTL`] markers — hashed like the
/// other URI markers so arbitrary actor URLs stay key-safe.
fn sig_fetch_negative_key(actor_url: &str) -> String {
    format!(
        "sigfetch-neg:{}",
        &hex::encode(Sha256::digest(actor_url.as_bytes()))[..32]
    )
}

/// The actor URL a signature's key-id parameter points at — the
/// document pubby fetches (and caches) to verify. Handles both
/// draft-cavage (`keyId` on `Signature`) and RFC 9421 (`keyid` inside
/// `Signature-Input` parameters). `None` when no parseable `http(s)`
/// key id is present — verification will fail on its own, and a
/// garbage extraction must never gate it early.
fn signature_actor_url(headers: &HashMap<String, String>) -> Option<String> {
    for name in ["signature", "signature-input"] {
        let Some(value) = headers.get(name) else {
            continue;
        };
        let lower = value.to_lowercase();
        let Some(pos) = lower.find("keyid=") else {
            continue;
        };
        let rest = value[pos + "keyid=".len()..].trim_start();
        let id = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.find('"').map(|end| &quoted[..end])
        } else {
            let end = rest
                .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
                .unwrap_or(rest.len());
            Some(&rest[..end])
        }?;
        let actor = id.split('#').next().unwrap_or(id).trim();
        if actor.starts_with("https://") || actor.starts_with("http://") {
            return Some(actor.to_string());
        }
    }
    None
}

/// Whether `actor_url` has a fresh positive `actor_cache` row — the
/// same 24h window pubby's `fetch_actor` reads. A negative marker must
/// not outlive a cache write from another path (e.g. a queued
/// `RemoteActorFetch` that succeeded after the signature-time fetch
/// failed), or a verifiable signature would be treated as absent.
async fn sig_actor_cached(db: &sqlx::PgPool, actor_url: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM actor_cache WHERE actor_id = $1 \
         AND fetched_at > now() - interval '24 hours')",
    )
    .bind(actor_url)
    .fetch_one(db)
    .await
    .unwrap_or(false)
}

/// Whether a verification error means the signer's actor document
/// couldn't be fetched at all (unreachable origin, invalid doc) — the
/// only failure worth negative-caching. Bad signatures, missing keys
/// and ownership mismatches say nothing about fetchability.
fn is_actor_fetch_failure(e: &pubby_sdk::ActivityPubError) -> bool {
    match e {
        pubby_sdk::ActivityPubError::SignatureVerification(m) => m.contains("Cannot fetch"),
        _ => false,
    }
}

/// Whether the signer's domain has an open *bulk* remote-gate circuit —
/// actor fetches to it are already failing, so the signature-time
/// fetch will almost certainly time out too. Status-class circuits
/// don't apply: they track `RemoteStatusFetch`, a different workload.
async fn bulk_circuit_open(state: &AppState, actor_url: &str) -> bool {
    let Some(domain) = super::moderation::actor_domain(actor_url) else {
        return false;
    };
    crate::jobs::remote_gate::circuit_open(
        &state.redis,
        &domain,
        crate::jobs::remote_gate::GateClass::Bulk,
    )
    .await
}

/// Verify the HTTP signature when present — `Some(actor_url)` on
/// success. `verify_fetch_signature` returns the signer's actor URL
/// (resolved from `keyId`). `pub(crate)` — the featured collection
/// gates on it too.
pub(crate) async fn verify_signature(
    state: &Arc<AppState>,
    ap: &ApContext,
    username: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Option<String> {
    if !headers.contains_key("signature") {
        return None;
    }
    let handler = crate::activitypub::handler_for_username(ap, username).ok()??;
    let map: HashMap<String, String> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let target = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());

    // Negative cache: a marker hit — or an open bulk circuit for the
    // signer's domain, meaning actor fetches there are already failing
    // — produces the same outcome as the failed fetch (no verified
    // signer) without paying the remote timeout on every request. A
    // fresh positive `actor_cache` row overrides both: pubby would
    // verify from cache without any fetch at all.
    let signer_actor = signature_actor_url(&map);
    let mut neg_key = None;
    if let Some(actor) = &signer_actor {
        let key = sig_fetch_negative_key(actor);
        let marked = state.cache.get(&key).await.ok().flatten().is_some();
        let circuit_open = !marked && bulk_circuit_open(state, actor).await;
        if (marked || circuit_open) && !sig_actor_cached(&state.db, actor).await {
            if circuit_open {
                let _ = state
                    .cache
                    .set(&key, &json!({"s": "neg"}), SIG_FETCH_NEGATIVE_TTL)
                    .await;
            }
            return None;
        }
        neg_key = Some(key);
    }

    let result = sync_call(move || handler.verify_fetch_signature("GET", &target, &map)).await;
    match result {
        Ok(actor) => Some(actor),
        Err(e) => {
            if let Some(key) = neg_key {
                if is_actor_fetch_failure(&e) {
                    let _ = state
                        .cache
                        .set(&key, &json!({"s": "neg"}), SIG_FETCH_NEGATIVE_TTL)
                        .await;
                }
            }
            None
        }
    }
}

fn json_response(doc: Value) -> Response {
    let mut doc = doc;
    // Event documents carry the FEP-8a8e context (plan.md §5.1);
    // everything else keeps pubby's.
    doc["@context"] = if doc.get("type").and_then(|v| v.as_str()) == Some("Event") {
        context::event_context()
    } else {
        pubby_sdk::model::ap_context()
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/activity+json")],
        Json(doc),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Event `attendees`/`organizers` collections (FEP-8a8e §7.1)
// ---------------------------------------------------------------------------

/// `GET /ap/users/{u}/statuses/{id}/{attendees,organizers}` —
/// `OrderedCollection` of actor URIs. `totalItems` is always public;
/// `orderedItems` is gated by `events.attendees_visibility`:
/// `public` → everyone, `participants` → accepted/tentative
/// participants and the organizer, `organizers` → organizer only,
/// anything else → empty. The `organizers` collection is public.
/// Signer-gated documents are never edge-cached.
pub async fn serve_event_collection(
    State(state): State<Arc<AppState>>,
    Path((username, status_id, kind)): Path<(String, i64, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    match event_collection_inner(&state, &username, status_id, &kind, &headers, &uri).await {
        Ok((doc, public)) => {
            super::edge::apply(json_response(doc), public, super::edge::anonymous(&headers))
        }
        Err(e) => {
            // Only genuine misses are cacheable 404s — a transient
            // failure must not be frozen at the edge as "not found".
            let not_found = matches!(e, Error::NotFound(_));
            if !not_found {
                tracing::warn!(status_id, kind, error = %e, "event collection failed");
            }
            super::edge::apply(
                (if not_found {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                })
                .into_response(),
                not_found,
                super::edge::anonymous(&headers),
            )
        }
    }
}

async fn event_collection_inner(
    state: &Arc<AppState>,
    username: &str,
    status_id: i64,
    kind: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(Value, bool)> {
    let Some(ap) = &state.ap else {
        return Err(Error::NotFound("ap".into()));
    };
    let ctx = PostContext::new(state);
    let status = posts::load_any(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if !status.local || status.tombstone || !posts::is_event(&status) {
        return Err(Error::NotFound("status".into()));
    }
    let event = sqlx::query_as::<_, crate::events::model::EventRow>(
        "SELECT * FROM events WHERE status_id = $1",
    )
    .bind(status_id)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or_else(|| Error::NotFound("event".into()))?;
    let base = status.uri.clone().unwrap_or_default();
    let coll_uri = format!("{base}/{kind}");

    // Verified signer → local account (for the participants gate).
    let signer = verify_signature(state, ap, username, headers, uri).await;
    let signer_account = match &signer {
        Some(actor) => signer_account_id(&ctx, actor).await?,
        None => None,
    };

    let (total, items): (i64, Vec<String>) = if kind == "organizers" {
        // Public: disclosed organizers plus the status author.
        let mut actors: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT COALESCE(a.actor_id, o.uri) FROM event_organizers o \
             LEFT JOIN accounts a ON a.id = o.account_id WHERE o.status_id = $1 \
             ORDER BY o.position",
        )
        .bind(status_id)
        .fetch_all(&ctx.db)
        .await?
        .into_iter()
        .flatten()
        .collect();
        if let Some(author) =
            sqlx::query_scalar::<_, String>("SELECT actor_id FROM accounts WHERE id = $1")
                .bind(status.account_id)
                .fetch_optional(&ctx.db)
                .await?
        {
            if !actors.contains(&author) {
                actors.insert(0, author);
            }
        }
        let n = actors.len() as i64;
        (n, actors)
    } else {
        // attendees — states that mean "going".
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT p.account_id, p.state FROM event_participations p \
             WHERE p.status_id = $1 AND p.role <> 'organizer' \
               AND p.state IN ('accepted','tentative')",
        )
        .bind(status_id)
        .fetch_all(&ctx.db)
        .await?;
        let total = rows.len() as i64;
        let allowed = match event.attendees_visibility {
            crate::events::model::AttendeesVisibility::Public => true,
            crate::events::model::AttendeesVisibility::Participants => {
                signer_account.is_some_and(|sid| {
                    sid == status.account_id || rows.iter().any(|(id, _)| *id == sid)
                })
            }
            crate::events::model::AttendeesVisibility::Organizers => {
                signer_account.is_some_and(|sid| sid == status.account_id)
            }
        };
        if !allowed {
            return Ok((
                json!({
                    "id": coll_uri,
                    "type": "OrderedCollection",
                    "totalItems": total,
                    "orderedItems": [],
                }),
                false,
            ));
        }
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        let actors: Vec<String> = if ids.is_empty() {
            vec![]
        } else {
            sqlx::query_scalar::<_, String>("SELECT actor_id FROM accounts WHERE id = ANY($1)")
                .bind(&ids)
                .fetch_all(&ctx.db)
                .await?
        };
        (total, actors)
    };

    Ok((
        json!({
            "id": coll_uri,
            "type": "OrderedCollection",
            "totalItems": total,
            "orderedItems": items,
        }),
        // Items are only anonymous-visible under `public` attendees
        // visibility (or the organizers collection, which is public).
        kind == "organizers"
            || event.attendees_visibility == crate::events::model::AttendeesVisibility::Public,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn signature_actor_url_parses_draft_cavage_keyid() {
        // The production scheme: `keyId` first among comma-separated
        // quoted params on `Signature`.
        let map = headers(&[(
            "signature",
            "keyId=\"https://remote.example/users/alice#main-key\",algorithm=\"rsa-sha256\",signature=\"abc\"",
        )]);
        assert_eq!(
            signature_actor_url(&map).as_deref(),
            Some("https://remote.example/users/alice")
        );
    }

    #[test]
    fn signature_actor_url_parses_rfc9421_keyid() {
        // RFC 9421: covered components then `;`-separated params on
        // `Signature-Input`.
        let map = headers(&[(
            "signature-input",
            "sig1=(\"@method\" \"@authority\");created=1700000000;keyid=\"https://remote.example/ap/users/bob#ed25519-key\";alg=\"ed25519\"",
        )]);
        assert_eq!(
            signature_actor_url(&map).as_deref(),
            Some("https://remote.example/ap/users/bob")
        );
    }

    #[test]
    fn signature_actor_url_rejects_garbage() {
        // No key id at all and a non-URL key id both yield `None` —
        // never a bogus cache key.
        assert_eq!(
            signature_actor_url(&headers(&[("signature", "algorithm=\"rsa-sha256\"")])),
            None
        );
        let map = headers(&[("signature", "keyId=\"not-a-url#k\"")]);
        assert_eq!(signature_actor_url(&map), None);
        // A key id without a fragment still resolves to the actor.
        let map = headers(&[("signature", "keyId=\"https://x.example/a\"")]);
        assert_eq!(
            signature_actor_url(&map).as_deref(),
            Some("https://x.example/a")
        );
    }
}
