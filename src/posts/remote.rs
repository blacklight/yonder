//! Remote status materialization — inbound `Create`/`Update`/`Delete`
//! for `Note`/`Question` objects (epic 12).
//!
//! Called from `activitypub::incoming::on_activity_fn` via
//! `sync_block` (it does sqlx I/O the sync hook can't); it answers
//! `PassThrough` so pubby's own interaction bookkeeping still runs.
//!
//! **Ownership gate** (before any write): `activity.actor` must equal
//! the object's `attributedTo` for Create/Update/Delete, and
//! Update/Delete must additionally match the `account_id` already
//! stored for the target `statuses.uri`. Bare-URI Deletes resolve
//! ownership from the existing row. Mismatches are logged and skipped.

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::activitypub::{lookup, moderation, ApContext, ApCore};
use crate::cache::RedisCache;
use crate::jobs::{JobContext, JobHandler, JobKind};
use crate::Result;

/// The object types we materialize — Notes, Questions and the
/// Mastodon-compatible Article/Page plus media/event shapes (Audio,
/// Video, Event — Songhive/Funkwhale tracks, PeerTube videos,
/// Mobilizon events) all degrade to notes. `pub(crate)` —
/// `search::resolve` accepts the same set for fetched documents.
pub(crate) const NOTE_TYPES: &[&str] = &[
    "Note", "Question", "Article", "Page", "Audio", "Video", "Event",
];

/// Entry point from `on_activity_fn`: decide whether `activity` is a
/// status-bearing Create/Update/Delete and materialize it. Returns
/// `false` when the activity isn't ours to handle (caller passes
/// through without side effects either way).
pub async fn handle(core: &ApCore, activity: &pubby_sdk::Activity, raw: &Value) -> bool {
    let object = raw.get("object").cloned().unwrap_or(Value::Null);
    match activity.activity_type.as_str() {
        "Create" | "Update" => {
            let object_type = object.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !NOTE_TYPES.contains(&object_type) {
                return false;
            }
            match upsert(core, activity, &object).await {
                Err(e) => tracing::warn!(error = %e, "remote status upsert failed"),
                // Materialized remote posts fan out to local followers
                // exactly like local ones — but only when the object is
                // new. A redelivered Create (or an Update materializing
                // a known row) must not re-notify subscribers.
                Ok(is_new) if is_new => {
                    if let Some(uri) = object.get("id").and_then(|v| v.as_str()) {
                        match super::load_by_uri(&core.db, uri).await {
                            Ok(Some(status)) if !status.tombstone => {
                                crate::timelines::fanout::enqueue(&core.dispatcher, status.id)
                                    .await;
                                let ctx = crate::timelines::stream::post_ctx_for_core(core);
                                crate::timelines::stream::publish_update(&ctx, &status).await;
                                crate::notifications::emit::emit_for_status(
                                    &core.db,
                                    &core.dispatcher,
                                    &status,
                                )
                                .await;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(uri, error = %e, "post-create status reload failed")
                            }
                        }
                    }
                }
                Ok(_) => {}
            }
            true
        }
        "Delete" => {
            // Bare-URI Deletes resolve through the stored row.
            let id = object.as_str().map(str::to_string).or_else(|| {
                object
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
            let Some(id) = id else { return false };
            match tombstone(core, activity, &id).await {
                Ok(done) => done,
                Err(e) => {
                    tracing::warn!(error = %e, "remote status delete failed");
                    true
                }
            }
        }
        _ => false,
    }
}

/// `attributedTo` → actor URL (string, `{id}`, or first array item).
fn attributed_to(object: &Value) -> Option<String> {
    match object.get("attributedTo") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("id").and_then(|v| v.as_str()).map(str::to_string),
        Some(Value::Array(a)) => a.first().and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        }),
        _ => None,
    }
}

/// `object.url` → the web permalink. AP allows `url` as a string, a
/// `Link`-shaped object, or an array of them (Songhive/Funkwhale
/// `Audio` objects carry one Link per representation — the media file
/// plus the `text/html` track page). Prefer the HTML link when
/// several are present; fall back to the object `id`.
fn object_url(object: &Value, object_id: &str) -> String {
    fn href(v: &Value) -> Option<&str> {
        match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(_) => v.get("href").and_then(|h| h.as_str()),
            _ => None,
        }
    }
    let picked = match object.get("url") {
        Some(Value::Array(links)) => links
            .iter()
            .find(|l| l.get("mediaType").and_then(|m| m.as_str()) == Some("text/html"))
            .and_then(href)
            .or_else(|| links.iter().find_map(href)),
        Some(v) => href(v),
        None => None,
    };
    picked.unwrap_or(object_id).to_string()
}

/// The best playable `url` Link for a `Video`/`Audio` object with no
/// `attachment` (PeerTube): `video/*` progressive first, then HLS
/// (`application/x-mpegURL`), then `audio/*`. Tracker/`text/html`
/// links are skipped.
struct PlayableLink {
    href: String,
    media_type: String,
    kind: &'static str,
}

fn playable_link(object: &Value) -> Option<PlayableLink> {
    let links = object.get("url").and_then(|v| v.as_array())?;
    let mut hls: Option<PlayableLink> = None;
    let mut audio: Option<PlayableLink> = None;
    for l in links {
        let Some(href) = l.get("href").and_then(|v| v.as_str()) else {
            continue;
        };
        let mt = l.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");
        if mt.starts_with("video/") {
            return Some(PlayableLink {
                href: href.to_string(),
                media_type: mt.to_string(),
                kind: "video",
            });
        }
        if matches!(
            mt,
            "application/x-mpegURL" | "application/vnd.apple.mpegurl"
        ) && hls.is_none()
        {
            hls = Some(PlayableLink {
                href: href.to_string(),
                media_type: mt.to_string(),
                kind: "video",
            });
        } else if mt.starts_with("audio/") && audio.is_none() {
            audio = Some(PlayableLink {
                href: href.to_string(),
                media_type: mt.to_string(),
                kind: "audio",
            });
        }
    }
    hls.or(audio)
}

/// Poster art for a remote attachment — surfaced as `preview_url`
/// (Mastodon renders it as cover art in its audio player / as the
/// `<video>` poster). Precedence: the attachment's own `preview`/
/// `image`/`icon`, then the object's, then — for Songhive/Funkwhale
/// `Audio` — the nested `track`/`track.album` documents. Each field
/// may be a string, a `Link`/`Image` object (`url` or `href`), or an
/// array of those (the largest rendition wins — PeerTube `icon`
/// lists thumbnails small→large).
fn preview_art_url(att: &Value, object: &Value) -> Option<String> {
    fn pick(v: Option<&Value>) -> Option<String> {
        match v {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Object(_)) => v
                .and_then(|o| o.get("url").or_else(|| o.get("href")))
                .and_then(|u| pick(Some(u))),
            Some(Value::Array(a)) => a.iter().find_map(|i| pick(Some(i))),
            _ => None,
        }
    }
    // For `Image`-object arrays (e.g. PeerTube `icon`) prefer the
    // largest rendition by area; unscored items keep array order.
    fn best(v: Option<&Value>) -> Option<String> {
        match v {
            Some(Value::Array(a)) if a.len() > 1 => {
                let area = |i: &Value| {
                    let w = i.get("width").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let h = i.get("height").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    (w * h) as i64
                };
                a.iter()
                    .max_by_key(|i| area(i))
                    .and_then(|i| pick(Some(i)))
                    .or_else(|| a.iter().find_map(|i| pick(Some(i))))
            }
            other => pick(other),
        }
    }
    fn art(doc: &Value) -> Option<String> {
        ["preview", "image", "icon"]
            .iter()
            .find_map(|k| best(doc.get(*k)))
    }
    art(att)
        .or_else(|| art(object))
        .or_else(|| object.get("track").and_then(art))
        .or_else(|| {
            object
                .get("track")
                .and_then(|t| t.get("album"))
                .and_then(art)
        })
}

/// Bare `accounts` upsert for a remote actor + `RemoteActorFetch`
/// enrichment — the `handle_flag` pattern (no network I/O).
/// `pub(crate)` — the interaction handlers reuse it (epic 13).
pub(crate) async fn ensure_account(core: &ApCore, actor_url: &str) -> Result<i64> {
    crate::activitypub::lookup::upsert_remote_stub(&core.db, &core.dispatcher, actor_url).await
}

/// The followers-collection URL `addressing` produces — used to
/// recognize followers-only inbound posts.
fn followers_url(actor_id: &str) -> String {
    format!("{}/followers", actor_id.trim_end_matches('/'))
}

/// Infer stored visibility from the activity+object `to`/`cc` fields:
/// Public in `to` → `public`; Public only in `cc` → `unlisted`; a
/// followers collection with no Public → `followers_only`; otherwise
/// actor/mention-limited → `direct`.
fn infer_visibility(
    activity: &pubby_sdk::Activity,
    object: &Value,
    actor_id: &str,
) -> &'static str {
    // Activity addressing is parsed on the `Activity`; object
    // addressing supplements it (some servers address only the inner
    // object).
    let mut to = activity.to.clone();
    let mut cc = activity.cc.clone();
    to.extend(str_list(object.get("to")));
    cc.extend(str_list(object.get("cc")));
    let public = "https://www.w3.org/ns/activitystreams#Public";
    if to.iter().any(|u| u == public) {
        return "public";
    }
    if cc.iter().any(|u| u == public) {
        return "unlisted";
    }
    let followers = followers_url(actor_id);
    if to.iter().chain(cc.iter()).any(|u| *u == followers) {
        return "followers_only";
    }
    "direct"
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Create/Update materialization. The ownership gate runs before any
/// write: `activity.actor` must equal `attributedTo`, and an Update
/// must match the stored owner. Returns `true` when the object was
/// previously unknown and a new row was materialized — fan-out and
/// stream hooks key off it so a redelivered Create doesn't re-notify.
async fn upsert(core: &ApCore, activity: &pubby_sdk::Activity, object: &Value) -> Result<bool> {
    let Some(author) = attributed_to(object) else {
        return Ok(false);
    };
    if author != activity.actor {
        tracing::warn!(
            actor = %activity.actor, attributed = %author,
            "spoofed remote status mutation dropped"
        );
        return Ok(false);
    }
    let object_id = object
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if object_id.is_empty() {
        return Ok(false);
    }
    let existing = super::load_by_uri(&core.db, &object_id).await?;
    let is_new = existing.is_none();
    if let Some(existing) = &existing {
        // Update only: stored owner must match, and the incoming
        // `updated` must be newer (idempotent redeliveries).
        let owner_actor: String = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
            .bind(existing.account_id)
            .fetch_one(&core.db)
            .await?;
        if owner_actor != activity.actor {
            tracing::warn!(actor = %activity.actor, uri = %object_id, "spoofed Update dropped");
            return Ok(false);
        }
        if activity.activity_type == "Update" {
            let incoming = object.get("updated").and_then(pubby_sdk::model::parse_dt);
            let stored_newest = existing.edited_at.unwrap_or(existing.updated_at);
            if let Some(incoming) = incoming {
                if incoming <= stored_newest {
                    return Ok(false);
                }
            }
        }
    } else if activity.activity_type == "Update" {
        // An Update for an unknown object materializes like a Create
        // (some servers never send the Create).
    }

    let account_id = ensure_account(core, &author).await?;
    let visibility = infer_visibility(activity, object, &author);
    let content_src = object
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // Most servers send `content` as HTML; PeerTube (and some others)
    // declare `mediaType` and send Markdown or plain-text source —
    // run those through the same render pipeline locals use so links,
    // formatting and line breaks survive instead of rendering raw.
    let media_type = object
        .get("mediaType")
        .and_then(|v| v.as_str())
        .map(|m| {
            m.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let render_type = match media_type.as_str() {
        "text/markdown" | "text/x-markdown" => "text/markdown",
        "text/plain" => "text/plain",
        _ => "text/html",
    };
    let object_type = object.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // Mastodon's "converted" types (long-form + media shapes): the
    // `name` headline and `summary` teaser join the status *text*
    // with the permalink — `summary` is never a CW there.
    let converted = matches!(
        object_type,
        "Article" | "Page" | "Audio" | "Video" | "Event"
    );
    // AS2 `summary` is HTML (WriteFreely sends
    // `<p><img …> […]</p>`); `spoiler_text` and the converted teaser
    // are plain text — strip it or the markup renders verbatim.
    let summary_text = object
        .get("summary")
        .and_then(|v| v.as_str())
        .map(|s| super::render::html_to_text(&super::render::sanitize_html(s, &core.config)))
        .unwrap_or_default()
        .chars()
        .take(2000)
        .collect::<String>();
    let mut content_hashtags: Vec<String> = Vec::new();
    let mut content_html = if render_type == "text/html" {
        super::render::sanitize_html(
            content_src,
            &crate::config::Config {
                content: core.config.content.clone(),
                ..Default::default()
            },
        )
    } else {
        let rendered =
            super::render::render(content_src, render_type, &[], &core.base_url, &core.config);
        content_hashtags = rendered.hashtags;
        rendered.html
    };
    let url = object_url(object, &object_id);
    if converted {
        // Mastodon converts these to headline + teaser + permalink;
        // keep the rendered body too — headline, teaser, body, then
        // the link footer (which the link-preview job turns into the
        // status card, picking up the article's header image).
        let mut wrapped = String::new();
        if let Some(name) = object
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|n| !n.is_empty())
        {
            wrapped.push_str("<p><strong>");
            wrapped.push_str(&pubby_sdk::content::html_escape(name));
            wrapped.push_str("</strong></p>");
        }
        if !summary_text.is_empty() {
            wrapped.push_str("<p>");
            wrapped.push_str(&pubby_sdk::content::html_escape(&summary_text));
            wrapped.push_str("</p>");
        }
        wrapped.push_str(&content_html);
        wrapped.push_str("<p>");
        wrapped.push_str(&pubby_sdk::render_link_anchor(
            &url,
            None,
            Some(url.as_str()),
        ));
        wrapped.push_str("</p>");
        content_html = wrapped;
    }
    let source_text = object
        .get("source")
        .and_then(|s| s.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .chars()
        .take(20000)
        .collect::<String>();
    // Most servers send no `source`; the sanitized HTML is then the only
    // text, and `content` must hold it or the status is unsearchable.
    let content = if source_text.is_empty() {
        super::render::html_to_text(&content_html)
    } else {
        source_text
    };
    // A `source`-supplied text skips `html_to_text`, so the converted
    // permalink footer never reached `content` — append it or the
    // link-preview extractor (which reads `content`) finds no URL.
    let content = if converted && !content.contains(&url) {
        format!("{content} {url}")
    } else {
        content
    };
    // Converted objects put the teaser in the text — no CW.
    let spoiler = (!converted && !summary_text.is_empty()).then_some(summary_text.clone());
    let sensitive = object
        .get("sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let language = object
        .get("language")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            // `contentMap` → first language key.
            object
                .get("contentMap")
                .and_then(|m| m.as_object())
                .and_then(|m| m.keys().next())
                .cloned()
        });
    // Quote target (epic 13): FEP-0449 fields first, then the
    // Pleroma `RE:` link convention as a fallback.
    let quote_target_uri = pubby_sdk::extract_quote_target(object)
        .or_else(|| crate::interactions::quote::extract_re_link(&content_html));
    let (quote_of_id, quote_uri_col): (Option<i64>, Option<String>) = match &quote_target_uri {
        Some(uri) => match super::load_by_uri(&core.db, uri).await? {
            Some(q) => (Some(q.id), None),
            None => (None, Some(uri.clone())),
        },
        None => (None, None),
    };
    let in_reply_to_uri = object
        .get("inReplyTo")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let (reply_id, reply_account) = match &in_reply_to_uri {
        Some(uri) => match super::load_by_uri(&core.db, uri).await? {
            Some(p) => (Some(p.id), Some(p.account_id)),
            None => (None, None),
        },
        None => (None, None),
    };
    let published = object.get("published").and_then(pubby_sdk::model::parse_dt);
    let updated = object.get("updated").and_then(pubby_sdk::model::parse_dt);

    // One transaction for the whole materialization — a mid-way
    // failure never leaves a half-written status.
    let mut tx = core.db.begin().await?;

    // Upsert the statuses row (ON CONFLICT uri → update). `edited_at`
    // keeps its stored value when the Update carries no `updated`.
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, content, content_html, content_type, language, \
             visibility, spoiler_text, sensitive, local, uri, url, in_reply_to_id, \
             in_reply_to_account_id, quote_of_id, quote_uri, object_type, \
             created_at, edited_at) \
         VALUES (timestamp_id('statuses', LEAST(COALESCE($12, now()), now())), \
                 $1,$2,$3,'text/plain',$4,$5,$6,$7,false,$8,$9,$10,$11,$14,$15,$16, \
                 LEAST(COALESCE($12, now()), now()), $13) \
         ON CONFLICT (uri) DO UPDATE SET \
             content = EXCLUDED.content, content_html = EXCLUDED.content_html, \
             language = EXCLUDED.language, visibility = EXCLUDED.visibility, \
             spoiler_text = EXCLUDED.spoiler_text, sensitive = EXCLUDED.sensitive, \
             in_reply_to_id = EXCLUDED.in_reply_to_id, \
             in_reply_to_account_id = EXCLUDED.in_reply_to_account_id, \
             quote_of_id = EXCLUDED.quote_of_id, \
             quote_uri = EXCLUDED.quote_uri, \
             object_type = EXCLUDED.object_type, \
             edited_at = COALESCE(EXCLUDED.edited_at, statuses.edited_at), \
             updated_at = now() \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&content)
    .bind(&content_html)
    .bind(&language)
    .bind(visibility)
    .bind(&spoiler)
    .bind(sensitive)
    .bind(&object_id)
    .bind(&url)
    .bind(reply_id)
    .bind(reply_account)
    .bind(published)
    .bind(updated)
    .bind(quote_of_id)
    .bind(&quote_uri_col)
    .bind(object.get("type").and_then(|v| v.as_str()))
    .fetch_one(&mut *tx)
    .await?;

    // Sync mentions: `Mention` tags → known accounts.
    sqlx::query("DELETE FROM status_mentions WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    for tag in object
        .get("tag")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if tag.get("type").and_then(|t| t.as_str()) == Some("Mention") {
            if let Some(href) = tag.get("href").and_then(|v| v.as_str()) {
                if let Some((aid, _, _, _)) = mentions_account(&mut tx, href).await? {
                    sqlx::query(
                        "INSERT INTO status_mentions (status_id, account_id) \
                         VALUES ($1,$2) ON CONFLICT DO NOTHING",
                    )
                    .bind(status_id)
                    .bind(aid)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
    }
    // Mentions are final — refresh the conversation key inside the
    // materialization transaction (epic 25; covers remote create and
    // update — the visibility upsert precedes this point).
    super::conversations::refresh_conversation_key(&mut tx, status_id).await?;
    // Hashtags → tags/status_tags; remote `Emoji` tags are
    // collected for post-commit recording — emoji must never fail
    // or poison the ingest transaction (epic 20).
    let mut remote_emoji: Vec<(String, String)> = Vec::new();
    sqlx::query("DELETE FROM status_tags WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    for tag in object
        .get("tag")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if tag.get("type").and_then(|t| t.as_str()) == Some("Emoji") {
            let name = tag.get("name").and_then(|v| v.as_str());
            let icon = tag
                .get("icon")
                .and_then(|i| i.get("url"))
                .and_then(|v| v.as_str())
                .or_else(|| tag.get("icon").and_then(|v| v.as_str()));
            if let (Some(name), Some(url)) = (name, icon) {
                remote_emoji.push((name.trim_matches(':').to_string(), url.to_string()));
            }
        }
        if tag.get("type").and_then(|t| t.as_str()) == Some("Hashtag") {
            if let Some(name) = tag
                .get("name")
                .and_then(|v| v.as_str())
                .map(|n| n.trim_start_matches('#').to_lowercase())
                .filter(|n| !n.is_empty())
            {
                let tag_id: i64 = sqlx::query_scalar(
                    "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) \
                     DO UPDATE SET name = EXCLUDED.name RETURNING id",
                )
                .bind(&name)
                .fetch_one(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO status_tags (status_id, tag_id) VALUES ($1,$2) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(status_id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    // Hashtags the renderer found in markdown/plain `content` (e.g.
    // PeerTube `text/markdown` descriptions carry literal `#tags` the
    // `tag` array may not list).
    for name in content_hashtags {
        let tag_id: i64 = sqlx::query_scalar(
            "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) \
             DO UPDATE SET name = EXCLUDED.name RETURNING id",
        )
        .bind(&name)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO status_tags (status_id, tag_id) VALUES ($1,$2) \
             ON CONFLICT DO NOTHING",
        )
        .bind(status_id)
        .bind(tag_id)
        .execute(&mut *tx)
        .await?;
    }
    // Media attachments — remote_url rows only while
    // `media.fetch_remote_media` is off (the fetch runs after
    // `tx.commit()` when it is on). On
    // Update, rows the object no longer names are dropped so stale
    // attachments don't linger.
    struct RemoteAtt {
        kind: &'static str,
        url: String,
        media_type: String,
        description: Option<String>,
        preview_art: Option<String>,
    }
    let mut atts: Vec<RemoteAtt> = Vec::new();
    for att in object
        .get("attachment")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let url = att.get("url").and_then(|v| v.as_str());
        let Some(url) = url else { continue };
        let media_type = att.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");
        let kind = if media_type.starts_with("image/") {
            "image"
        } else if media_type.starts_with("video/") {
            "video"
        } else if media_type.starts_with("audio/") {
            "audio"
        } else {
            continue;
        };
        let description = att
            .get("name")
            .or_else(|| att.get("summary"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(5000).collect::<String>());
        let preview_art = preview_art_url(att, object);
        atts.push(RemoteAtt {
            kind,
            url: url.to_string(),
            media_type: media_type.to_string(),
            description,
            preview_art,
        });
    }
    // `Video`/`Audio` objects may carry no `attachment` at all —
    // PeerTube puts the playable source in `url` Links (`video/*`,
    // or HLS `application/x-mpegURL`) and the poster in `icon`.
    // Synthesize one attachment from the best playable link so the
    // status gets a real media element. A real `attachment` of the
    // same A/V kind wins when present.
    let is_av = matches!(
        object.get("type").and_then(|v| v.as_str()),
        Some("Video") | Some("Audio")
    );
    if is_av && !atts.iter().any(|a| a.kind != "image") {
        if let Some(link) = playable_link(object) {
            let description = object
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.chars().take(5000).collect::<String>());
            atts.push(RemoteAtt {
                kind: link.kind,
                url: link.href,
                media_type: link.media_type,
                description,
                preview_art: preview_art_url(&Value::Null, object),
            });
        }
    }
    let mut remote_urls: Vec<String> = Vec::new();
    for a in &atts {
        remote_urls.push(a.url.clone());
        sqlx::query(
            "INSERT INTO media_attachments \
                 (account_id, status_id, type, remote_url, content_type, description, \
                  preview_remote_url) \
             SELECT $1,$2,$3,$4,$5,$6,$7 WHERE NOT EXISTS ( \
                 SELECT 1 FROM media_attachments \
                 WHERE status_id = $2 AND remote_url = $4 \
             )",
        )
        .bind(account_id)
        .bind(status_id)
        .bind(a.kind)
        .bind(&a.url)
        .bind(&a.media_type)
        .bind(&a.description)
        .bind(&a.preview_art)
        .execute(&mut *tx)
        .await?;
        // Re-ingested Updates keep the existing row (media must not be
        // re-fetched) but the poster may have changed — converge it.
        sqlx::query(
            "UPDATE media_attachments SET preview_remote_url = $3 \
             WHERE status_id = $1 AND remote_url = $2 \
               AND preview_remote_url IS DISTINCT FROM $3",
        )
        .bind(status_id)
        .bind(&a.url)
        .bind(&a.preview_art)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "DELETE FROM media_attachments \
         WHERE status_id = $1 AND remote_url IS NOT NULL \
           AND NOT (remote_url = ANY($2))",
    )
    .bind(status_id)
    .bind(&remote_urls)
    .execute(&mut *tx)
    .await?;
    // Question → polls/poll_options (votes stay remote).
    if object.get("type").and_then(|v| v.as_str()) == Some("Question") {
        sync_poll(&mut tx, status_id, object).await?;
    }
    // Reply counter on the parent: only moves when the linkage changed —
    // a new reply bumps it, an Update reparenting shifts it, and a
    // redelivered Create (same parent) is a no-op.
    let old_reply: Option<i64> = existing.as_ref().and_then(|e| e.in_reply_to_id);
    if old_reply != reply_id {
        if let Some(old) = old_reply {
            sqlx::query(
                "UPDATE statuses SET replies_count = GREATEST(replies_count - 1, 0) WHERE id = $1",
            )
            .bind(old)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(new_p) = reply_id {
            sqlx::query("UPDATE statuses SET replies_count = replies_count + 1 WHERE id = $1")
                .bind(new_p)
                .execute(&mut *tx)
                .await?;
        }
    }
    // The author's post counter moves only when a brand-new row was
    // materialized — Update redeliveries are a no-op.
    if existing.is_none() {
        sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
    }
    // Quote counters: only moves when the target changed. The prior
    // row's `quote_of_id` was read before the upsert.
    let old_quote: Option<i64> = existing.as_ref().and_then(|e| e.quote_of_id);
    if old_quote != quote_of_id {
        if let Some(old) = old_quote {
            sqlx::query(
                "UPDATE statuses SET quotes_count = GREATEST(quotes_count - 1, 0) WHERE id = $1",
            )
            .bind(old)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(new_q) = quote_of_id {
            sqlx::query("UPDATE statuses SET quotes_count = quotes_count + 1 WHERE id = $1")
                .bind(new_q)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;

    // Epic 20 — record remote `Emoji` tags post-commit, on the pool:
    // a bad row here must never abort ingest (the status is durable).
    if !remote_emoji.is_empty() {
        let domain: Option<String> =
            sqlx::query_scalar("SELECT domain FROM accounts WHERE id = $1")
                .bind(account_id)
                .fetch_optional(&core.db)
                .await
                .ok()
                .flatten()
                .flatten();
        if let Some(domain) = domain {
            for (code, url) in &remote_emoji {
                if let Err(e) =
                    crate::emoji::model::upsert_remote(&core.db, code, &domain, url).await
                {
                    tracing::warn!(code, domain, error = %e, "remote emoji upsert failed");
                }
            }
        }
    }

    // Epic 18: remote-media caching — post-commit so the rows the
    // job will read are visible. A queue failure must not fail the
    // committed ingest: rows keep `remote_url`, and the next ingest
    // (or a re-run of the same activity) re-enqueues.
    if let Err(e) = enqueue_remote_media_fetches(core, status_id).await {
        tracing::warn!(status_id, error = %e, "remote media fetch enqueue failed");
    }

    // Link previews get the same `FetchLinkPreview` enqueue locals get
    // on create/edit — materialization is the remote equivalent. Only
    // fresh rows and accepted Updates enqueue: a redelivered Create
    // changed nothing (a stale Update returned before this point).
    if is_new || activity.activity_type == "Update" {
        if let Ok(Some(status)) = super::load(&core.db, status_id).await {
            let media_ids: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM media_attachments WHERE status_id = $1")
                    .bind(status_id)
                    .fetch_all(&core.db)
                    .await
                    .unwrap_or_default();
            let ctx = crate::timelines::stream::post_ctx_for_core(core);
            super::preview::enqueue(&ctx, &status, &media_ids).await;
        }
    }

    // Late-arriving quoted posts back-fill pending quotes — the
    // counter bump applies to *this* status only (it is the quoted
    // target); the pending quoters point at it via quote_uri.
    backfill_pending_quotes(&core.db, status_id, &object_id).await?;

    // A still-pending quote target gets an active fetch — passive
    // backfill alone never resolves quotes whose target's server
    // doesn't federate with us. Fresh rows and accepted Updates only:
    // a redelivered Create changed nothing.
    if is_new || activity.activity_type == "Update" {
        if let Some(uri) = &quote_uri_col {
            if let Err(e) = enqueue_quote_resolve(core, uri).await {
                tracing::warn!(status_id, uri, error = %e, "quote resolve enqueue failed");
            }
        }
    }
    Ok(is_new)
}

/// Link every pending quoter (`quote_uri` = `uri`, `quote_of_id`
/// NULL) to the now-materialized `target_id`, bumping the target's
/// `quotes_count` by the number linked.
pub(crate) async fn backfill_pending_quotes(
    db: &sqlx::PgPool,
    target_id: i64,
    uri: &str,
) -> Result<()> {
    sqlx::query(
        "WITH upd AS ( \
             UPDATE statuses SET quote_of_id = $1, quote_uri = NULL \
             WHERE quote_uri = $2 AND quote_of_id IS NULL \
             RETURNING quote_of_id \
         ) \
         UPDATE statuses SET quotes_count = quotes_count + (SELECT count(*) FROM upd) \
         WHERE id = $1",
    )
    .bind(target_id)
    .bind(uri)
    .execute(db)
    .await?;
    Ok(())
}

/// Epic 18: remote-media caching. With `media.fetch_remote_media`
/// on, every attachment row still pointing at the remote gets a
/// `RemoteMediaFetch` job — including on re-ingest, which
/// re-downloads whatever the cleanup sweep evicted. Rows already
/// carrying `file_path` are skipped by the predicate, so a re-ingest
/// of a cached status enqueues nothing. `pub` so the ingest-dedup
/// predicate is directly testable.
pub async fn enqueue_remote_media_fetches(core: &ApCore, status_id: i64) -> Result<()> {
    if !core.config.media.fetch_remote_media {
        return Ok(());
    }
    // Epic 21: `reject_media` (or suspended) domains keep no remote
    // bytes — skip the fetch jobs entirely.
    let author_domain: Option<String> = sqlx::query_scalar(
        "SELECT a.domain FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE s.id = $1",
    )
    .bind(status_id)
    .fetch_one(&core.db)
    .await?;
    if let Some(domain) = author_domain {
        if crate::moderation::blocks::rejects_media(&core.db, &domain)
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }
    }
    let pending: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM media_attachments \
         WHERE status_id = $1 AND remote_url IS NOT NULL AND file_path IS NULL",
    )
    .bind(status_id)
    .fetch_all(&core.db)
    .await?;
    for media_id in pending {
        core.dispatcher
            .enqueue(
                crate::jobs::JobKind::RemoteMediaFetch,
                serde_json::json!({ "media_id": media_id }),
            )
            .await?;
    }
    Ok(())
}

/// `status_mentions` account lookup for a Mention `href`.
async fn mentions_account(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor_url: &str,
) -> Result<Option<(i64, String, Option<String>, String)>> {
    let row =
        sqlx::query("SELECT id, username, domain, actor_id FROM accounts WHERE actor_id = $1")
            .bind(actor_url)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.map(|r| {
        (
            r.get("id"),
            r.get("username"),
            r.get("domain"),
            r.get("actor_id"),
        )
    }))
}

/// Materialize a remote `Question`: `oneOf`/`anyOf` options and
/// `endTime`; `replies.totalItems` seeds `votes_count`.
async fn sync_poll(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    status_id: i64,
    object: &Value,
) -> Result<()> {
    let options_value = object
        .get("oneOf")
        .or_else(|| object.get("anyOf"))
        .and_then(|v| v.as_array());
    let Some(options) = options_value else {
        return Ok(());
    };
    if options.is_empty() {
        return Ok(());
    }
    let multiple = object.get("anyOf").is_some();
    let expires_at = object.get("endTime").and_then(pubby_sdk::model::parse_dt);
    let poll_id: i64 = sqlx::query_scalar(
        "INSERT INTO polls (status_id, expires_at, multiple) VALUES ($1,$2,$3) \
         ON CONFLICT (status_id) DO UPDATE SET expires_at = EXCLUDED.expires_at, \
             multiple = EXCLUDED.multiple, updated_at = now() RETURNING id",
    )
    .bind(status_id)
    .bind(expires_at)
    .bind(multiple)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("UPDATE statuses SET poll_id = $2 WHERE id = $1")
        .bind(status_id)
        .bind(poll_id)
        .execute(&mut **tx)
        .await?;
    for (idx, option) in options.iter().enumerate() {
        let title = option
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect::<String>();
        let votes = option
            .get("replies")
            .and_then(|r| r.get("totalItems"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        sqlx::query(
            "INSERT INTO poll_options (poll_id, idx, title, votes_count) VALUES ($1,$2,$3,$4) \
             ON CONFLICT (poll_id, idx) DO UPDATE SET title = EXCLUDED.title, \
                 votes_count = EXCLUDED.votes_count",
        )
        .bind(poll_id)
        .bind(idx as i32)
        .bind(&title)
        .bind(votes)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// `Delete` — tombstone by `uri`; bare-URI objects resolve ownership
/// from the stored row.
async fn tombstone(core: &ApCore, activity: &pubby_sdk::Activity, object_id: &str) -> Result<bool> {
    let Some(existing) = super::load_by_uri(&core.db, object_id).await? else {
        return Ok(false);
    };
    let owner_actor: String = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(existing.account_id)
        .fetch_one(&core.db)
        .await?;
    if owner_actor != activity.actor {
        tracing::warn!(actor = %activity.actor, uri = %object_id, "spoofed Delete dropped");
        return Ok(true);
    }
    if existing.tombstone {
        return Ok(true);
    }
    // Tombstone in place — a remote Delete doesn't need our Delete
    // re-published, and remote media has no local objects to detach.
    sqlx::query("DELETE FROM media_attachments WHERE status_id = $1")
        .bind(existing.id)
        .execute(&core.db)
        .await?;
    sqlx::query(
        "UPDATE statuses SET content = '', content_html = '', spoiler_text = NULL, \
             sensitive = false, tombstone = true, updated_at = now() WHERE id = $1",
    )
    .bind(existing.id)
    .execute(&core.db)
    .await?;
    // The deletion releases the author's post counter; a deleted reply
    // releases its parent's counter; same for a deleted quote post's
    // target.
    sqlx::query(
        "UPDATE accounts SET statuses_count = GREATEST(statuses_count - 1, 0) WHERE id = $1",
    )
    .bind(existing.account_id)
    .execute(&core.db)
    .await?;
    if let Some(parent) = existing.in_reply_to_id {
        sqlx::query(
            "UPDATE statuses SET replies_count = GREATEST(replies_count - 1, 0) WHERE id = $1",
        )
        .bind(parent)
        .execute(&core.db)
        .await?;
    }
    if let Some(quoted) = existing.quote_of_id {
        sqlx::query(
            "UPDATE statuses SET quotes_count = GREATEST(quotes_count - 1, 0) WHERE id = $1",
        )
        .bind(quoted)
        .execute(&core.db)
        .await?;
    }
    crate::timelines::fanout::remove_quiet(
        &core.db,
        &core.redis,
        &core.config.timelines,
        existing.id,
        existing.account_id,
    )
    .await;
    crate::timelines::stream::publish_delete(&core.db, &core.bus, &existing).await;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Pending-quote resolution (epic 13 follow-up)
// ---------------------------------------------------------------------------

/// How long a "queued" marker suppresses repeat enqueues — covers job
/// latency plus the retry window; after expiry a fresh ingest or read
/// may enqueue again.
const QUEUE_MARKER_TTL: Duration = Duration::from_secs(10 * 60);
/// A resolved (or already-cached) target suppresses refetches for a
/// day — the `load_by_uri` hit makes it belt-and-braces anyway.
const OK_MARKER_TTL: Duration = Duration::from_secs(24 * 3600);
/// A definitively-unfetchable target (Gone, untrusted, suspended)
/// backs off for an hour before another ingest/read may retry.
const FAIL_MARKER_TTL: Duration = Duration::from_secs(3600);

/// Redis marker key deduping fetches per quote-target URI.
fn fetch_marker_key(uri: &str) -> String {
    format!(
        "statusfetch:{}",
        &hex::encode(Sha256::digest(uri.as_bytes()))[..32]
    )
}

/// Redis marker key for media self-heal refreshes — a separate
/// namespace so a settled quote marker never blocks a media re-fetch
/// (and vice versa).
fn media_refresh_marker_key(uri: &str) -> String {
    format!(
        "statusfetch-media:{}",
        &hex::encode(Sha256::digest(uri.as_bytes()))[..32]
    )
}

/// A media-heal refresh backs off a week per row: a successful one
/// lands the attachment (or the object type) and stops the gate, a
/// failed one retries at most weekly per viewed status.
const MEDIA_REFRESH_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Enqueue a `RemoteStatusFetch` with `refresh` for a remote A/V row
/// that materialized without media — rows ingested before `url`-Link
/// synthesis existed (or a remote that only later added the source)
/// get the playable attachment + poster on next read. Deduped by the
/// media marker for [`MEDIA_REFRESH_TTL`], so repeat views don't
/// pile up fetches.
pub async fn enqueue_media_refresh(core: &ApCore, uri: &str) -> Result<()> {
    let cache = RedisCache::new(core.redis.clone());
    let key = media_refresh_marker_key(uri);
    if cache.get(&key).await.ok().flatten().is_some() {
        return Ok(());
    }
    core.dispatcher
        .enqueue(
            JobKind::RemoteStatusFetch,
            json!({"uri": uri, "refresh": true}),
        )
        .await?;
    let _ = cache
        .set(&key, &json!({"s": "queued"}), MEDIA_REFRESH_TTL)
        .await;
    Ok(())
}

/// Enqueue a `RemoteStatusFetch` for a pending quote target — deduped
/// by the marker, so repeat ingests of quoters (and status reads of
/// rows left pending before the job existed) don't pile up jobs.
/// When the target is already materialized — lost the ingest race or
/// arrived since — quoters are linked inline instead.
pub async fn enqueue_quote_resolve(core: &ApCore, uri: &str) -> Result<()> {
    if let Some(target) = super::load_by_uri(&core.db, uri).await? {
        backfill_pending_quotes(&core.db, target.id, uri).await?;
        return Ok(());
    }
    let cache = RedisCache::new(core.redis.clone());
    let key = fetch_marker_key(uri);
    if cache.get(&key).await.ok().flatten().is_some() {
        return Ok(());
    }
    core.dispatcher
        .enqueue(JobKind::RemoteStatusFetch, json!({"uri": uri}))
        .await?;
    let _ = cache
        .set(&key, &json!({"s": "queued"}), QUEUE_MARKER_TTL)
        .await;
    Ok(())
}

/// `JobKind::RemoteStatusFetch` — `{uri}`: signed-fetch a remote
/// status object, materialize it through the same path as an inbound
/// `Create`, and link every quoter that was parked on `quote_uri`.
///
/// Definitive misses (Gone, untrusted doc, suspended domain,
/// non-status object) ack and set a `fail` marker — never a retry
/// storm. Transient fetch failures return `Err` so the queue's
/// backoff applies.
pub struct RemoteStatusFetchJob {
    ap: Option<ApContext>,
}

impl RemoteStatusFetchJob {
    /// Create the handler; `ap` is `Some` when federation is enabled
    /// (remote ingest implies it, but a `None` handler acks cleanly).
    pub fn new(ap: Option<ApContext>) -> Self {
        RemoteStatusFetchJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for RemoteStatusFetchJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(uri) = payload.get("uri").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(ap) = &self.ap else {
            return Ok(());
        };
        // `refresh` jobs deliberately re-fetch a materialized row (the
        // media self-heal) — skip the already-cached and
        // settled-marker shortcuts; the enqueue-side media marker
        // bounds them instead.
        let refresh = payload
            .get("refresh")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cache = RedisCache::new(ctx.redis.clone());
        let key = fetch_marker_key(uri);
        if !refresh {
            // Already materialized — link pending quoters without a
            // fetch (covers the ingest-race and a sibling job that got
            // here first).
            if let Some(target) = super::load_by_uri(&ctx.db, uri).await? {
                backfill_pending_quotes(&ctx.db, target.id, uri).await?;
                let _ = cache.set(&key, &json!({"s": "ok"}), OK_MARKER_TTL).await;
                return Ok(());
            }
            if let Some(marker) = cache.get(&key).await.ok().flatten() {
                // A previous run already settled this URI.
                if let Some("ok" | "fail") = marker.get("s").and_then(Value::as_str) {
                    return Ok(());
                }
            }
        }
        let domain = moderation::actor_domain(uri).unwrap_or_default();
        if crate::search::resolve::domain_suspended(&ctx.db, &domain).await? {
            let _ = cache
                .set(&key, &json!({"s": "fail"}), FAIL_MARKER_TTL)
                .await;
            return Ok(());
        }
        match lookup::fetch_remote_object_detailed(ap, uri).await? {
            lookup::RemoteFetch::Gone => {
                let _ = cache
                    .set(&key, &json!({"s": "fail"}), FAIL_MARKER_TTL)
                    .await;
                Ok(())
            }
            // Transport-level failure — retry through the queue's
            // backoff budget.
            lookup::RemoteFetch::Failed => Err(crate::Error::Internal(format!(
                "remote status fetch failed: {uri}"
            ))),
            lookup::RemoteFetch::Doc(doc) => {
                match crate::search::resolve::materialize_remote(&ctx.db, ap, uri, &doc).await? {
                    // The fetched doc's `id` can differ from the URI
                    // the quoter stored (e.g. `quoteUrl` vs `id`), so
                    // back-fill by the requested URI explicitly — the
                    // ingest-side CTE only matches `object_id`.
                    Some(crate::search::resolve::ResolvedHit::Status(id)) => {
                        backfill_pending_quotes(&ctx.db, id, uri).await?;
                        // Refresh re-materializes an existing row, so
                        // the ingest-side `is_new`/Update gate skipped
                        // the preview enqueue — run it here or
                        // self-healed statuses never get their card.
                        if refresh {
                            if let Ok(Some(status)) = super::load(&ctx.db, id).await {
                                let media_ids: Vec<i64> = sqlx::query_scalar(
                                    "SELECT id FROM media_attachments WHERE status_id = $1",
                                )
                                .bind(id)
                                .fetch_all(&ctx.db)
                                .await
                                .unwrap_or_default();
                                let pctx = super::PostContext::from_job(ctx, self.ap.clone());
                                super::preview::enqueue(&pctx, &status, &media_ids).await;
                            }
                        }
                        let _ = cache.set(&key, &json!({"s": "ok"}), OK_MARKER_TTL).await;
                        Ok(())
                    }
                    // Settled but never a status — an actor document,
                    // an untrusted or rejected Note.
                    _ => {
                        let _ = cache
                            .set(&key, &json!({"s": "fail"}), FAIL_MARKER_TTL)
                            .await;
                        Ok(())
                    }
                }
            }
        }
    }
}

/// Register this module's job handler.
pub(crate) fn register(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(
        JobKind::RemoteStatusFetch,
        RemoteStatusFetchJob::new(ap.cloned()),
    );
}
