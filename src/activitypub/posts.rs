//! Status objects on the wire — Note/Question building, Create/Update/
//! Delete envelopes, and the `/ap/users/{u}/statuses/{id}` object GET
//! (epic 12).
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
//! resolved via `can_see`. Only public/unlisted documents enter the
//! shared `doc_cache`; private documents render per-request.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;

use pubby_sdk::Object;

use crate::http::AppState;
use crate::posts::{self, mentions::ResolvedMention, PostContext, StatusRow};
use crate::{Error, Result};

use super::{actors::sync_call, delivery, ApContext};

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

/// `attachment` Documents for the status's media rows.
async fn attachments(ctx: &PostContext, status: &StatusRow) -> Result<Vec<Value>> {
    let base = ctx.base_url();
    let media = posts::entity::media_for_status(ctx, status.id).await?;
    Ok(media
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
        .collect())
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

/// The full object document (Note/Question) for a stored status —
/// served verbatim by the object GET and embedded in Create/Update.
pub async fn build_object(ctx: &PostContext, status: &StatusRow) -> Result<Value> {
    let (_username, actor_id) = author_parts(ctx, status.account_id).await?;
    let followers = format!("{actor_id}/followers");
    let mentions = mention_audience(ctx, status.id).await?;
    let mention_urls = posts::mentions::mention_actor_urls(&mentions);
    let (to, cc) = posts::addressing(&status.visibility, &followers, &mention_urls);

    let mut object = Object::new(status.uri.clone().unwrap_or_default());
    object.object_type = if status.poll_id.is_some() {
        "Question"
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
    object.summary = status.spoiler_text.clone();
    object.sensitive = status.sensitive;
    object.tag = posts::mentions::mention_tags(&mentions);
    object.tag.extend(pubby_sdk::build_hashtag_tags(
        hashtags(ctx, status.id).await?,
        |name| format!("{}/tags/{name}", ctx.base_url()),
    ));
    object.attachment = attachments(ctx, status).await?;
    object.to = to;
    object.cc = cc;
    object.language = status.language.clone();

    let mut doc = object.to_json();
    // `source` (markdown round-trip) — `Object` has no field for it.
    doc["source"] = json!({
        "content": status.content,
        "mediaType": status.content_type,
    });
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
                let mime = match meta.url.rsplit('.').next() {
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("gif") => "image/gif",
                    Some("webp") => "image/webp",
                    Some("avif") => "image/avif",
                    _ => "image/png",
                };
                tags.push(json!({
                    "type": "Emoji",
                    "id": format!("{}/emoji/{}", base.trim_end_matches('/'), meta.id),
                    "name": format!(":{}:", meta.shortcode),
                    "icon": { "type": "Image", "mediaType": mime, "url": meta.url },
                }));
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
    pubby_sdk::quotes::allow_public_quotes(&mut doc);
    // Quotes (epic 13): stamp every FEP-0449 spelling. `quote_of_id`
    // resolves through the target row; `quote_uri` covers remote
    // targets not materialized yet.
    let quote_uri: Option<String> = match status.quote_of_id {
        Some(qid) => {
            sqlx::query_scalar::<_, Option<String>>("SELECT uri FROM statuses WHERE id = $1")
                .bind(qid)
                .fetch_optional(&ctx.db)
                .await?
                .flatten()
        }
        None => status.quote_uri.clone(),
    };
    if let Some(uri) = quote_uri {
        pubby_sdk::quotes::set_quote_target(&mut doc, &uri);
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
    let username = username_of(ctx, status.account_id).await?;
    let doc = build_object(ctx, status).await?;
    let obj = object_for_activity(&doc);
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let mut activity = sync_call(move || handler.outbox.build_create_activity(&obj));
    // Inject the fields `Object::to_json` can't carry.
    if let Some(object) = activity.get_mut("object") {
        merge_object_extras(object, &doc);
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
    let username = username_of(ctx, status.account_id).await?;
    let doc = build_object(ctx, status).await?;
    let obj = object_for_activity(&doc);
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let mut activity = sync_call(move || handler.outbox.build_update_activity_obj(&obj));
    if let Some(object) = activity.get_mut("object") {
        merge_object_extras(object, &doc);
    }
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// Delete → `Delete(object_uri)` to the stored audience's inboxes.
pub async fn publish_delete(ap: &ApContext, ctx: &PostContext, status: &StatusRow) -> Result<()> {
    let username = username_of(ctx, status.account_id).await?;
    let Some(uri) = status.uri.clone() else {
        return Ok(());
    };
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
    let activity = sync_call(move || handler.outbox.build_delete_activity_obj(&uri));
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// Copy the doc-only fields (`source`, poll fields, quote fields,
/// `interactionPolicy`) `Object::to_json` dropped onto the activity's
/// embedded object.
fn merge_object_extras(target: &mut Value, doc: &Value) {
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
        "interactionPolicy",
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
/// here).
async fn signer_account_id(ctx: &PostContext, actor_url: &str) -> Result<Option<i64>> {
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
    match serve_object_inner(&state, &username, status_id, &headers, &uri).await {
        Ok(doc) => json_response(doc),
        Err(Error::Unauthorized) => StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Gate + build the object document.
async fn serve_object_inner(
    state: &Arc<AppState>,
    username: &str,
    status_id: i64,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<Value> {
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
    if public_doc && state.config.federation.authorized_fetch && signer.is_none() {
        return Err(Error::Unauthorized);
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
        return cached.ok_or_else(|| Error::NotFound("status".into()));
    }

    // followers_only / direct — the verified signer must be allowed.
    let Some(signer) = signer else {
        return Err(Error::NotFound("status".into()));
    };
    let account = signer_account_id(&ctx, &signer).await?;
    match account {
        Some(id) if posts::can_see(&ctx.db, &status, Some(id)).await? => {
            build_object(&ctx, &status).await
        }
        _ => Err(Error::NotFound("status".into())),
    }
}

/// Verify the HTTP signature when present — `Some(actor_url)` on
/// success. `verify_fetch_signature` returns the signer's actor URL
/// (resolved from `keyId`). `pub(crate)` — the featured collection
/// gates on it too.
pub(crate) async fn verify_signature(
    _state: &Arc<AppState>,
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
    sync_call(move || handler.verify_fetch_signature("GET", &target, &map)).ok()
}

fn json_response(doc: Value) -> Response {
    let mut doc = doc;
    doc["@context"] = pubby_sdk::model::ap_context();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/activity+json")],
        Json(doc),
    )
        .into_response()
}
