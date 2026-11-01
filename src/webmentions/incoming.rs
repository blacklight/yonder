//! Incoming Webmention receiver (`POST /webmentions`) and the
//! `WebmentionIncoming` job.
//!
//! The endpoint is deliberately thin — mirror of the AP inbox edge:
//! validate the form fields, bound the target to this instance,
//! reject suspended source domains, enqueue, answer 202. The job
//! does the expensive part (fetch the source, verify the backlink,
//! parse Microformats2) through the SDK processor, then materializes
//! the mention as a reply status authored by a pseudo-actor.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;
use sqlx::PgPool;
use webmentions_sdk::{Webmention, WebmentionError};

use crate::{
    activitypub::ApContext,
    http::AppState,
    jobs::{JobContext, JobHandler, JobKind},
    posts::StatusRow,
    Result,
};

use super::{actor_id_for, enabled, status_uri_for, storage::PgWebmentionsStorage};

/// `POST /webmentions` — accept a mention for asynchronous
/// processing (HTTP 202). `source`/`target` arrive form-encoded per
/// the W3C recommendation.
pub async fn receive(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    if !enabled(&state.config()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let (Some(source), Some(target)) = (form.get("source"), form.get("target")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing source or target parameter"})),
        )
            .into_response();
    };
    for value in [source, target] {
        let ok = url::Url::parse(value)
            .map(|u| {
                matches!(u.scheme(), "http" | "https")
                    && !u.host_str().unwrap_or_default().is_empty()
            })
            .unwrap_or(false);
        if !ok {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "source and target must be absolute http(s) URLs"})),
            )
                .into_response();
        }
    }
    let target_url = url::Url::parse(target).expect("validated above");
    let instance = url::Url::parse(&crate::activitypub::federation_base_url(&state.config()))
        .expect("federation base URL");
    if target_url.host_str().unwrap_or_default().to_lowercase()
        != instance.host_str().unwrap_or_default().to_lowercase()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "target URL is not on this instance"})),
        )
            .into_response();
    }
    let source_host = url::Url::parse(source)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    match crate::moderation::blocks::is_suspended(&state.db, &source_host).await {
        Ok(true) => return StatusCode::FORBIDDEN.into_response(),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, "domain suspension check failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    if let Err(e) = state
        .dispatcher
        .enqueue(
            JobKind::WebmentionIncoming,
            json!({"source": source, "target": target}),
        )
        .await
    {
        tracing::warn!(error = %e, "could not enqueue incoming webmention");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "queued", "source": source, "target": target})),
    )
        .into_response()
}

/// `JobKind::WebmentionIncoming` — `{"source": …, "target": …}`.
/// Verifies the source through the SDK, then materializes (or
/// retracts) the reply row.
pub struct IncomingJob {
    ap: Option<ApContext>,
}

impl IncomingJob {
    /// Create the handler; `ap` supplies the streaming bus and the
    /// timeline fan-out context when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        Self { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for IncomingJob {
    async fn run(&self, ctx: &JobContext, payload: &serde_json::Value) -> Result<()> {
        if !enabled(&ctx.config) {
            return Ok(());
        }
        let (Some(source), Some(target)) = (
            payload.get("source").and_then(|v| v.as_str()),
            payload.get("target").and_then(|v| v.as_str()),
        ) else {
            return Ok(());
        };
        // The edge already checked, but queued payloads from before a
        // block could still land — re-verify the source domain.
        let source_host = url::Url::parse(source)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        if crate::moderation::blocks::is_suspended(&ctx.db, &source_host).await? {
            tracing::info!(source, "dropping webmention from suspended domain");
            return Ok(());
        }

        let storage = Arc::new(PgWebmentionsStorage::new(ctx.db.clone()));
        let handler = super::build_handler(&ctx.config, storage, None, None);
        let (source_owned, target_owned) = (source.to_string(), target.to_string());
        let outcome = crate::activitypub::actors::sync_call(move || {
            handler.process_incoming_webmention(Some(&source_owned), Some(&target_owned))
        });
        match outcome {
            // Verified + stored — materialize the reply.
            Ok(Some(mention)) => materialize(ctx, self.ap.as_ref(), &mention).await,
            // The source is gone or stopped linking — retract any
            // materialized reply.
            Ok(None) => retract(ctx, self.ap.as_ref(), source, target).await,
            // Terminal rejections (invalid payload, SSRF refusal,
            // source doesn't link back) — the sender already has its
            // 202; log and drop.
            Err(WebmentionError::Webmention { .. } | WebmentionError::InvalidValue(_)) => {
                tracing::info!(source, target, "webmention rejected");
                Ok(())
            }
            // Transient fetch failures (timeouts, 5xx, DNS) propagate
            // so the job's retry budget engages — a swallowed error
            // drops a mention the sender believes was accepted.
            Err(WebmentionError::Http(msg)) => Err(crate::Error::Internal(format!(
                "webmention source fetch failed: {msg}"
            ))),
            Err(e) => {
                tracing::warn!(source, target, error = %e, "webmention processing failed");
                Ok(())
            }
        }
    }
}

/// Map a local target URL to the status it identifies.
///
/// Recognized shapes: the AP object URI
/// (`/ap/users/{u}/statuses/{id}`), the web permalink
/// (`/@{u}/{id}` — equal to `statuses.url`), and the SPA route
/// (`/statuses/{id}`). Only local, live statuses resolve — a mention
/// of anything else stays recorded in the ledger without a reply.
pub async fn resolve_target_status(db: &PgPool, target: &str) -> Result<Option<StatusRow>> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM statuses WHERE local AND NOT tombstone AND (uri = $1 OR url = $1)",
        crate::posts::STATUS_COLS
    ))
    .bind(target)
    .fetch_optional(db)
    .await?;
    if let Some(status) = row.as_ref().map(crate::posts::row_to_status) {
        return Ok(Some(status));
    }
    let Ok(parsed) = url::Url::parse(target) else {
        return Ok(None);
    };
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    let id: Option<i64> = match segments.as_slice() {
        ["ap", "users", _, "statuses", id] | ["statuses", id] => id.parse().ok(),
        _ => None,
    };
    match id {
        Some(id) => Ok(crate::posts::load(db, id).await?.filter(|s| s.local)),
        None => Ok(None),
    }
}

/// The pseudo-actor's display username for an author URL — the last
/// meaningful path segment (`/@alice` → `alice`), the host for bare
/// domains (`example.com` → `example.com` — the Bridgy-style
/// `example.com@example.com` handle).
fn author_username(author_url: &url::Url) -> String {
    let host = author_url.host_str().unwrap_or_default();
    let slug = author_url
        .path_segments()
        .into_iter()
        .flatten()
        .rfind(|s| !s.is_empty())
        .unwrap_or(host);
    slug.trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(200)
        .collect()
}

/// The identity key a pseudo-actor is keyed on — `author_url` when
/// the source supplies one, else the source page's origin.
fn author_key(mention: &Webmention) -> String {
    mention
        .author_url
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| {
            url::Url::parse(&mention.source)
                .ok()
                .map(|u| u.origin().ascii_serialization())
                .filter(|o| o != "null")
                .unwrap_or_else(|| mention.source.clone())
        })
}

/// Upsert the pseudo-actor account for a mention and return its id.
///
/// A `urn:yonder:webmention:` `actor_id` keeps the row unique across
/// authors while marking it as non-dereferenceable (remote-actor
/// refresh skips `urn:` ids). When the author's URL already matches
/// a known actor (`actor_id` or `url`) — e.g. the blog owner also has
/// a fediverse presence here — the real account is reused instead.
async fn ensure_webmention_actor(db: &PgPool, mention: &Webmention) -> Result<i64> {
    if let Some(author_url) = mention.author_url.as_deref().filter(|u| !u.is_empty()) {
        if let Some(id) =
            sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE actor_id = $1 OR url = $1")
                .bind(author_url)
                .fetch_optional(db)
                .await?
        {
            return Ok(id);
        }
    }

    let key = author_key(mention);
    let actor_id = actor_id_for(&key);
    let parsed =
        url::Url::parse(&key).unwrap_or_else(|_| url::Url::parse("https://invalid").unwrap());
    // Netloc (host[:port]) — the `accounts.domain` convention.
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(h), Some(p)) => format!("{}:{p}", h.to_lowercase()),
        (Some(h), None) => h.to_lowercase(),
        _ => String::new(),
    };
    let mut username = author_username(&parsed);
    let display_name = mention
        .author_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| Some(username.clone()));
    let profile_url = if key.starts_with("http") {
        Some(key.clone())
    } else {
        None
    };
    let actor_data = json!({
        "type": "Service",
        "name": display_name,
        "url": key,
        "yonder:webmention": true,
    });

    // A real account may already own the derived `username@host`
    // (the author's AP presence under the same handle); suffix the
    // pseudo-actor's name deterministically in that case.
    let taken = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE username = $1 \
         AND COALESCE(domain, '') = $2 AND actor_id <> $3)",
    )
    .bind(&username)
    .bind(&host)
    .bind(&actor_id)
    .fetch_one(db)
    .await?;
    if taken {
        username = format!(
            "{username}-{:.6}",
            &actor_id["urn:yonder:webmention:".len()..]
        );
    }

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO accounts (domain, username, actor_id, url, display_name, \
             avatar_remote_url, actor_data, discoverable, directory_opt_in) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,false,false) \
         ON CONFLICT (actor_id) DO UPDATE SET \
             url = COALESCE(EXCLUDED.url, accounts.url), \
             display_name = COALESCE(EXCLUDED.display_name, accounts.display_name), \
             avatar_remote_url = COALESCE(EXCLUDED.avatar_remote_url, accounts.avatar_remote_url), \
             updated_at = now() \
         RETURNING id",
    )
    .bind(&host)
    .bind(&username)
    .bind(&actor_id)
    .bind(&profile_url)
    .bind(&display_name)
    .bind(&mention.author_photo)
    .bind(&actor_data)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// The reply's display payload — the mention title as a linked
/// headline, the (sanitized) content or excerpt, and the source
/// permalink footer (which also feeds the link-preview extractor).
fn reply_content(config: &crate::config::Config, mention: &Webmention) -> (String, String) {
    let mut html = String::new();
    if let Some(title) = mention.title.as_deref().filter(|t| !t.is_empty()) {
        html.push_str(&format!(
            "<p><strong><a href=\"{}\">{}</a></strong></p>",
            pubby_sdk::content::html_escape(&mention.source),
            pubby_sdk::content::html_escape(title),
        ));
    }
    if let Some(content) = mention.content.as_deref().filter(|c| !c.is_empty()) {
        html.push_str(&crate::posts::render::sanitize_html(
            &content.chars().take(20_000).collect::<String>(),
            config,
        ));
    } else if let Some(excerpt) = mention.excerpt.as_deref().filter(|e| !e.is_empty()) {
        html.push_str(&format!(
            "<p>{}</p>",
            pubby_sdk::content::html_escape(excerpt)
        ));
    }
    html.push_str(&format!(
        "<p>{}</p>",
        pubby_sdk::render_link_anchor(&mention.source, None, Some(mention.source.as_str())),
    ));
    let mut content = crate::posts::render::html_to_text(&html)
        .chars()
        .take(19_000)
        .collect::<String>();
    // The rendered anchor's href isn't in the plain text — append it
    // so the link-preview job finds the source URL.
    if !content.contains(&mention.source) {
        content = format!("{content} {}", mention.source);
    }
    (content, html)
}

/// Materialize a verified mention as a reply status authored by its
/// pseudo-actor. Idempotent on the deterministic status `uri`:
/// re-verified sources update the row (source edits propagate),
/// brand-new pairs fan out like any remote reply.
async fn materialize(ctx: &JobContext, ap: Option<&ApContext>, mention: &Webmention) -> Result<()> {
    let Some(target) = resolve_target_status(&ctx.db, &mention.target).await? else {
        // Recorded in the ledger, but not a mentionable permalink —
        // nothing to materialize.
        return Ok(());
    };
    let account_id = ensure_webmention_actor(&ctx.db, mention).await?;
    let uri = status_uri_for(&mention.source, &mention.target);
    let existing = crate::posts::load_by_uri(&ctx.db, &uri).await?;
    // A tombstoned row (previously retracted source) resurrects as a
    // fresh reply — counters and notifications re-fire.
    let is_new = existing.as_ref().map(|e| e.tombstone).unwrap_or(true);
    let (content, content_html) = reply_content(&ctx.config, mention);
    let created = mention.published.unwrap_or_else(chrono::Utc::now);

    let mut tx = ctx.db.begin().await?;
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, content, content_html, content_type, \
             visibility, local, uri, url, in_reply_to_id, in_reply_to_account_id, \
             object_type, created_at) \
         VALUES (timestamp_id('statuses', LEAST($9, now())), \
                 $1,$2,$3,'text/plain',$4,false,$5,$6,$7,$8,'Note',LEAST($9, now())) \
         ON CONFLICT (uri) DO UPDATE SET \
             content = EXCLUDED.content, content_html = EXCLUDED.content_html, \
             visibility = EXCLUDED.visibility, url = EXCLUDED.url, \
             in_reply_to_id = EXCLUDED.in_reply_to_id, \
             in_reply_to_account_id = EXCLUDED.in_reply_to_account_id, \
             tombstone = false, \
             updated_at = now() \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&content)
    .bind(&content_html)
    .bind(&target.visibility)
    .bind(&uri)
    .bind(&mention.source)
    .bind(target.id)
    .bind(target.account_id)
    .bind(created)
    .fetch_one(&mut *tx)
    .await?;
    // The target's author is the implicit mention — it drives the
    // notification and the conversation key.
    sqlx::query("DELETE FROM status_mentions WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO status_mentions (status_id, account_id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(status_id)
    .bind(target.account_id)
    .execute(&mut *tx)
    .await?;
    crate::posts::conversations::refresh_conversation_key(&mut tx, status_id).await?;
    if is_new {
        sqlx::query("UPDATE statuses SET replies_count = replies_count + 1 WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE webmentions SET status_id = $1, account_id = $2 \
         WHERE source = $3 AND target = $4 AND direction = 'incoming'",
    )
    .bind(status_id)
    .bind(account_id)
    .bind(&mention.source)
    .bind(&mention.target)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let Some(status) = crate::posts::load(&ctx.db, status_id).await? else {
        return Ok(());
    };
    let post_ctx = match ap {
        Some(ap) => crate::timelines::stream::post_ctx_for_core(&ap.core),
        None => crate::posts::PostContext::from_job(ctx, None),
    };
    if is_new {
        crate::timelines::fanout::enqueue(&post_ctx.dispatcher, status.id).await;
        crate::timelines::stream::publish_update(&post_ctx, &status).await;
        crate::notifications::emit::emit_for_status(&ctx.db, &post_ctx.dispatcher, &status).await;
        crate::posts::preview::enqueue(&post_ctx, &status, &[]).await;
    } else {
        crate::timelines::stream::publish_status_update(&post_ctx, &status).await;
    }
    Ok(())
}

/// Retract a mention whose source disappeared or stopped linking —
/// tombstone the reply exactly like a remote Delete does.
async fn retract(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    source: &str,
    target: &str,
) -> Result<()> {
    let uri = status_uri_for(source, target);
    let Some(status) = crate::posts::load_by_uri(&ctx.db, &uri).await? else {
        return Ok(());
    };
    if status.tombstone {
        return Ok(());
    }
    sqlx::query("DELETE FROM media_attachments WHERE status_id = $1")
        .bind(status.id)
        .execute(&ctx.db)
        .await?;
    sqlx::query(
        "UPDATE statuses SET content = '', content_html = '', spoiler_text = NULL, \
             sensitive = false, tombstone = true, updated_at = now() WHERE id = $1",
    )
    .bind(status.id)
    .execute(&ctx.db)
    .await?;
    sqlx::query(
        "UPDATE accounts SET statuses_count = GREATEST(statuses_count - 1, 0) WHERE id = $1",
    )
    .bind(status.account_id)
    .execute(&ctx.db)
    .await?;
    if let Some(parent) = status.in_reply_to_id {
        sqlx::query(
            "UPDATE statuses SET replies_count = GREATEST(replies_count - 1, 0) WHERE id = $1",
        )
        .bind(parent)
        .execute(&ctx.db)
        .await?;
    }
    crate::timelines::fanout::remove_quiet(
        &ctx.db,
        &ctx.redis,
        &ctx.config.timelines,
        status.id,
        status.account_id,
    )
    .await;
    let bus = ap.map(|ap| ap.core.bus.clone()).unwrap_or_default();
    crate::timelines::stream::publish_delete(&ctx.db, &bus, &status).await;
    Ok(())
}
