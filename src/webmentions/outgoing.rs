//! Outgoing Webmention delivery — the `h-entry` source page
//! (`GET /webmentions/source/{id}`) and the `WebmentionOutgoing` job.
//!
//! When a local public/unlisted status is published, edited or
//! deleted, [`enqueue`] queues the job whenever it could carry
//! Webmention targets: links in the content, or a previously recorded
//! delivery (so edits and deletions retract what was sent). The job
//! feeds the status's rendered `h-entry` — the same document the
//! endpoint serves — to the SDK's outgoing processor, which diffs
//! the extracted targets against the stored `outgoing` ledger:
//! newly linked targets get `source`/`target` POSTed to their
//! discovered endpoints, removed links get a re-verification ping.

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde_json::json;
use webmentions_sdk::ContentTextFormat;

use crate::{
    activitypub::ApContext,
    http::AppState,
    jobs::{JobContext, JobHandler, JobKind},
    posts::{PostContext, StatusRow},
    Result,
};

use super::{
    enabled, endpoint_url, like_source_url, render, source_url, storage::PgWebmentionsStorage,
};

/// Statuses eligible for outgoing delivery.
fn deliverable(status: &StatusRow) -> bool {
    status.local && matches!(status.visibility.as_str(), "public" | "unlisted")
}

/// Whether a status was materialized from an incoming Webmention —
/// only those get interaction backfeed (remote AP statuses already
/// federate their own activities).
fn is_webmention_reply(status: &StatusRow) -> bool {
    status
        .uri
        .as_deref()
        .map(|u| u.starts_with("urn:yonder:webmention:"))
        .unwrap_or(false)
}

/// The remote permalinks of the statuses this one replies to, boosts
/// or quotes — `None` for local ones (same-instance targets stay in
/// the local pipeline and are excluded from delivery anyway).
async fn related_urls(db: &sqlx::PgPool, status: &StatusRow) -> Result<render::Interactions> {
    let ids: Vec<i64> = [
        status.in_reply_to_id,
        status.reblog_of_id,
        status.quote_of_id,
    ]
    .into_iter()
    .flatten()
    .collect();
    if ids.is_empty() {
        return Ok(render::Interactions::default());
    }
    let rows: Vec<(i64, bool, Option<String>)> =
        sqlx::query_as("SELECT id, local, url FROM statuses WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(db)
            .await?;
    let remote_url = |id: Option<i64>| {
        id.and_then(|id| {
            rows.iter()
                .find(|(rid, local, _)| *rid == id && !*local)
                .and_then(|(_, _, u)| u.clone())
        })
    };
    Ok(render::Interactions {
        reply: remote_url(status.in_reply_to_id),
        repost: remote_url(status.reblog_of_id),
        quote: remote_url(status.quote_of_id),
    })
}

/// Queue the `WebmentionOutgoing` job for a just-persisted local
/// status — best-effort, like every other publish fan-out.
///
/// Enqueued when the run could have work: a URL in a public/unlisted
/// post's text, a reply/boost/quote toward a remote status
/// (`in-reply-to`/`repost-of`/`quotation-of` backfeed), or a stored
/// outgoing delivery — a post whose links were all removed (or that
/// was deleted/private-ized) still needs the run so the SDK's diff
/// retracts delivered mentions.
pub async fn enqueue(ctx: &PostContext, status: &StatusRow) {
    if !enabled(&ctx.config) || !status.local {
        return;
    }
    // The probes are fail-open: a failed lookup can't prove there is
    // nothing to send, so on error the job is enqueued anyway — it
    // re-derives everything under its retry budget, while a dropped
    // enqueue would lose the delivery silently.
    let has_targets = deliverable(status)
        && (status.content.contains("http")
            || related_urls(&ctx.db, status)
                .await
                .map(|i| i.any())
                .unwrap_or(true));
    let has_stored = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM webmentions \
         WHERE source = $1 AND direction = 'outgoing' AND status <> 'deleted')",
    )
    .bind(source_url(&ctx.config, status.id))
    .fetch_one(&ctx.db)
    .await
    .unwrap_or(true);
    if !(has_targets || has_stored) {
        return;
    }
    if let Err(e) = ctx
        .dispatcher
        .enqueue(JobKind::WebmentionOutgoing, json!({"status_id": status.id}))
        .await
    {
        tracing::warn!(status_id = status.id, error = %e, "webmention outgoing enqueue failed");
    }
}

/// Load the h-card for an account — the `p-author` fragment of
/// outgoing source documents.
async fn author_card(
    db: &sqlx::PgPool,
    base_url: &str,
    account_id: i64,
) -> Result<render::AuthorCard> {
    let (username, display_name, avatar, noindex): (String, Option<String>, Option<String>, bool) =
        sqlx::query_as(
            "SELECT a.username, a.display_name, a.avatar_remote_url, \
             COALESCE(u.indexable, true) = false \
             FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
             WHERE a.id = $1",
        )
        .bind(account_id)
        .fetch_one(db)
        .await?;
    Ok(render::AuthorCard {
        profile_url: format!("{base_url}/@{username}"),
        name: display_name.unwrap_or_else(|| username.clone()),
        photo: avatar.filter(|a| !a.is_empty()),
        username,
        noindex,
    })
}

/// The `h-entry` document for a status — empty when it is not (or no
/// longer) deliverable so remote receivers retract stored mentions.
async fn source_document(ctx: &PostContext, status: &StatusRow) -> Result<String> {
    if !deliverable(status) || status.tombstone {
        return Ok(String::new());
    }
    let base = ctx.base_url();
    let author = author_card(&ctx.db, &base, status.account_id).await?;
    let (_, status_url) = ctx.status_urls(&author.username, status.id);
    let interactions = related_urls(&ctx.db, status).await?;
    let tags: Vec<String> = sqlx::query_scalar(
        "SELECT t.name FROM tags t JOIN status_tags st ON st.tag_id = t.id \
         WHERE st.status_id = $1 ORDER BY t.name",
    )
    .bind(status.id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(render::render_source_page(
        &base,
        &endpoint_url(&ctx.config),
        status,
        &status_url,
        &interactions,
        Some(&author),
        &tags,
    ))
}

/// Queue a `like-of` backfeed for `account_id`'s favourite on
/// `status` — or its retraction when the favourite is gone. Only
/// webmention-materialized statuses backfeed (remote AP statuses
/// already get the `Like` activity).
pub async fn enqueue_like(ctx: &PostContext, status: &StatusRow, account_id: i64) {
    if !enabled(&ctx.config) || status.local || !is_webmention_reply(status) {
        return;
    }
    let visible = matches!(status.visibility.as_str(), "public" | "unlisted");
    let has_stored = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM webmentions \
         WHERE source = $1 AND direction = 'outgoing' AND status <> 'deleted')",
    )
    .bind(like_source_url(&ctx.config, status.id, account_id))
    .fetch_one(&ctx.db)
    .await
    // Fail-open like `enqueue` — a transient error must not drop a
    // retraction run the ledger can't vouch for.
    .unwrap_or(true);
    if !(visible || has_stored) {
        return;
    }
    if let Err(e) = ctx
        .dispatcher
        .enqueue(
            JobKind::WebmentionOutgoing,
            json!({"like_status_id": status.id, "like_account_id": account_id}),
        )
        .await
    {
        tracing::warn!(status_id = status.id, error = %e, "webmention like enqueue failed");
    }
}

/// The `u-like-of` document for `account_id`'s favourite on
/// `status_id` — empty when there is no favourite or the status is
/// not a public webmention reply, so remote receivers retract stored
/// likes.
async fn like_document(ctx: &PostContext, status_id: i64, account_id: i64) -> Result<String> {
    let Some(status) = crate::posts::load_any(&ctx.db, status_id).await? else {
        return Ok(String::new());
    };
    let wm_reply = !status.local
        && is_webmention_reply(&status)
        && matches!(status.visibility.as_str(), "public" | "unlisted")
        && !status.tombstone;
    let liked_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT created_at FROM favourites WHERE account_id = $1 AND status_id = $2",
    )
    .bind(account_id)
    .bind(status_id)
    .fetch_optional(&ctx.db)
    .await?;
    let (Some(target), Some(liked_at)) = (status.url.clone(), liked_at) else {
        return Ok(String::new());
    };
    if !wm_reply {
        return Ok(String::new());
    }
    let base = ctx.base_url();
    let liker = author_card(&ctx.db, &base, account_id).await?;
    Ok(render::render_like_page(
        &endpoint_url(&ctx.config),
        &like_source_url(&ctx.config, status_id, account_id),
        &target,
        &liker,
        &liked_at,
    ))
}

/// `GET /webmentions/like/{status_id}/{account_id}` — the `u-like-of`
/// source document remote receivers fetch to verify a delivered
/// like. 404 when the target isn't a webmention reply, 410 when the
/// favourite is gone (retraction signal).
pub async fn serve_like(
    State(state): State<Arc<AppState>>,
    Path((status_id, account_id)): Path<(i64, i64)>,
) -> Response {
    if !enabled(&state.config()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ctx = PostContext::new(&state);
    match like_document(&ctx, status_id, account_id).await {
        Ok(html) if !html.is_empty() => {
            let mut resp = Html(html).into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(
                &webmentions_sdk::server::webmention_link_header_value(&endpoint_url(
                    &state.config(),
                )),
            ) {
                resp.headers_mut().append(axum::http::header::LINK, v);
            }
            resp
        }
        Ok(_) => {
            // A live webmention reply with no favourite → the like
            // was retracted (or never existed — 410 retracts either
            // way); anything else is simply not a like page.
            let wm_reply = crate::posts::load_any(&state.db, status_id)
                .await
                .ok()
                .flatten()
                .map(|s| is_webmention_reply(&s))
                .unwrap_or(false);
            if wm_reply {
                StatusCode::GONE.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /webmentions/source/{id}` — serve the `h-entry` document
/// remote receivers fetch to verify delivered mentions. Mirrors the
/// object endpoint's semantics: unknown/undeliverable → 404,
/// tombstoned → 410.
pub async fn serve_source(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    if !enabled(&state.config()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ctx = PostContext::new(&state);
    let status = match crate::posts::load_any(&state.db, id).await {
        Ok(s) => s,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(status) = status else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if status.tombstone {
        return StatusCode::GONE.into_response();
    }
    match source_document(&ctx, &status).await {
        Ok(html) if !html.is_empty() => {
            let mut resp = Html(html).into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(
                &webmentions_sdk::server::webmention_link_header_value(&endpoint_url(
                    &state.config(),
                )),
            ) {
                resp.headers_mut().append(axum::http::header::LINK, v);
            }
            resp
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The failures a `process_outgoing_webmentions` run swallowed: the
/// SDK reports them per-target via `log::info!` inside
/// `notify_added`/`notify_removed` and still returns `Ok`, so the job
/// would ack a run that delivered nothing — a transient discovery/POST
/// error would lose the mention silently, with no ledger row to diff
/// against on a later run. Recording them through `on_mention_failed`
/// lets [`OutgoingJob`] return `Err` and engage the retry budget.
type Failures = Arc<Mutex<Vec<String>>>;

/// `JobKind::WebmentionOutgoing` — `{"status_id": …}` for a status's
/// links, `{"like_status_id": …, "like_account_id": …}` for a
/// favourite's `like-of` backfeed. Runs the SDK's discovery +
/// delivery; an undeliverable source renders an empty document so
/// stored targets retract.
pub struct OutgoingJob {
    ap: Option<ApContext>,
}

impl OutgoingJob {
    /// Create the handler; `ap` is unused today but keeps the
    /// signature aligned with the other job handlers.
    pub fn new(ap: Option<ApContext>) -> Self {
        Self { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for OutgoingJob {
    async fn run(&self, ctx: &JobContext, payload: &serde_json::Value) -> Result<()> {
        if !enabled(&ctx.config) {
            return Ok(());
        }
        let post_ctx = PostContext::from_job(ctx, self.ap.clone());
        let (source, doc) =
            if let Some(status_id) = payload.get("like_status_id").and_then(|v| v.as_i64()) {
                let account_id = payload
                    .get("like_account_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default();
                let doc = like_document(&post_ctx, status_id, account_id).await?;
                (like_source_url(&ctx.config, status_id, account_id), doc)
            } else if let Some(status_id) = payload.get("status_id").and_then(|v| v.as_i64()) {
                // A briefly-missing row means the publishing transaction
                // hasn't committed — a transient error retries; anything
                // else is terminal.
                let Some(status) = crate::posts::load_any(&ctx.db, status_id).await? else {
                    return Err(crate::Error::Internal(format!(
                        "status {status_id} not found for outgoing webmentions"
                    )));
                };
                if !status.local {
                    return Ok(());
                }
                (
                    source_url(&ctx.config, status_id),
                    source_document(&post_ctx, &status).await?,
                )
            } else {
                return Ok(());
            };

        let failures: Failures = Arc::new(Mutex::new(Vec::new()));
        let storage = Arc::new(PgWebmentionsStorage::new(ctx.db.clone()));
        let on_failed: webmentions_sdk::MentionErrorCallback = {
            let failures = failures.clone();
            Arc::new(
                move |m: &webmentions_sdk::Webmention, e: &webmentions_sdk::WebmentionError| {
                    failures
                        .lock()
                        .unwrap()
                        .push(format!("{} -> {}: {e}", m.source, m.target));
                },
            )
        };
        let handler = super::build_handler(&ctx.config, storage, None, None, Some(on_failed));
        let log_source = source.clone();
        let result = crate::activitypub::actors::sync_call(move || {
            handler.process_outgoing_webmentions(&source, Some(&doc), Some(ContentTextFormat::Html))
        });
        match result {
            // Per-target failures surface through the observing
            // fetcher/storage rather than the return value: report
            // them so the retry budget re-drives delivery instead of
            // dropping the mentions.
            Ok(()) => {
                let failures = std::mem::take(&mut *failures.lock().unwrap());
                if failures.is_empty() {
                    Ok(())
                } else {
                    tracing::warn!(
                        source = %log_source,
                        failures = ?failures,
                        "outgoing webmentions: delivery failures"
                    );
                    Err(crate::Error::Internal(format!(
                        "outgoing webmentions: {} failed target(s): {}",
                        failures.len(),
                        failures.join("; ")
                    )))
                }
            }
            // Discovery/delivery errors are per-target inside the
            // processor; a bubbled error is the source-side storage
            // read — retryable.
            Err(e) => {
                tracing::warn!(source = %log_source, error = %e, "outgoing webmentions failed");
                Err(crate::Error::Internal(format!(
                    "outgoing webmentions failed: {e}"
                )))
            }
        }
    }
}
