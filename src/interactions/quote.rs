//! Quotes — `quote_id` create integration, the Pleroma `RE:` link
//! convention and the FEP-044f consent-aware `QuoteRequest` outbound.
//!
//! Yonder's quote policy is public-quotes-always: emitted objects
//! stamp `allow_public_quotes` (`activitypub::posts::build_object`)
//! and inbound `QuoteRequest`s are auto-accepted by pubby
//! (`auto_approve_quotes`). Outbound quotes publish a normal `Create`
//! carrying every FEP-0449 field spelling ([`set_quote_target`]) and
//! *also* send `QuoteRequest` to the remote quoted author —
//! consent-aware without blocking on the answer.

use serde_json::Value;

use crate::{
    activitypub::{delivery, posts as ap_posts, ApContext},
    jobs::{JobContext, JobHandler, JobKind, Registry},
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

use super::{account_parts, emit_notification_event, new_activity_id};

/// Validate a `quote_id` param: the target must exist, not be
/// tombstoned and be visible to the quoter. A boost row normalizes to
/// the original post. Returns the resolved target.
pub async fn validate_target(ctx: &PostContext, quote_id: i64, viewer: i64) -> Result<StatusRow> {
    let target = super::load_visible(ctx, quote_id, Some(viewer)).await?;
    match target.reblog_of_id {
        Some(orig) => super::load_visible(ctx, orig, Some(viewer)).await,
        None => Ok(target),
    }
}

/// The `RE:` paragraph Pleroma/Akkoma quote renderers scan for —
/// appended to the *rendered* HTML (never to `source`).
pub fn re_link_html(url: &str) -> String {
    format!(
        "<p>RE: {}</p>",
        pubby_sdk::render_link_anchor(url, None, None)
    )
}

/// Inbound fallback: some Pleroma-family posts carry no quote field —
/// scan `content` for `RE: <a href="…">`. Heuristic: the first `href`
/// after any `RE:` wins, so a post whose text mentions "RE:" followed
/// by an unrelated link could false-positive. Only consulted when no
/// FEP-0449 field is present, which bounds the exposure to
/// Pleroma-family posts.
pub fn extract_re_link(html: &str) -> Option<String> {
    let re_pos = html.find("RE:")?;
    let rest = &html[re_pos..];
    let href_start = rest.find("href=\"")? + "href=\"".len();
    let href_end = rest[href_start..].find('"')?;
    let url = &rest[href_start..href_start + href_end];
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

/// FEP-044f `QuoteRequest` to the remote quoted author, carrying our
/// Note document as the `instrument` — consent-aware: remote
/// auto-approvers (incl. Yonder) answer `Accept(QuoteAuthorization)`,
/// consent-requiring servers get the request. Best-effort; the quote
/// is never withheld pending approval.
pub async fn send_quote_request(
    ctx: &PostContext,
    ap: &ApContext,
    status: &StatusRow,
    target: &StatusRow,
) -> Result<()> {
    // A `local` quoting status never federates — the QuoteRequest
    // would carry its Note document to the remote author.
    if status.visibility == "local" {
        return Ok(());
    }
    if target.local {
        return Ok(());
    }
    let Some(quoted_uri) = target.uri.clone() else {
        return Ok(());
    };
    let (username, actor_id) = account_parts(ctx, status.account_id).await?;
    let (_, author_actor) = account_parts(ctx, target.account_id).await?;
    let handler = crate::activitypub::handler_for_username(ap, &username)?
        .ok_or_else(|| crate::Error::NotFound(format!("actor {username}")))?;
    let doc = ap_posts::build_object(ctx, status).await?;
    let activity = crate::activitypub::actors::sync_call(move || {
        handler.outbox.build_quote_request_activity(
            &quoted_uri,
            &doc,
            &author_actor,
            Some(&new_activity_id(&actor_id)),
            None,
        )
    });
    delivery::publish(ap, &username, &activity).await?;
    Ok(())
}

/// `JobKind::QuoteAuthorization` — stamp the accepted `QuoteAuthorization`
/// on the quoting status and fan out an `Update` carrying
/// `quoteAuthorization`. Mastodon-family instances keep a remote quote
/// pending until they see that stamp (or a valid approval on the object).
pub struct QuoteAuthorizationJob {
    ap: ApContext,
}

impl QuoteAuthorizationJob {
    /// Create the handler bound to `ap`.
    pub fn new(ap: ApContext) -> Self {
        QuoteAuthorizationJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for QuoteAuthorizationJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let status_id = payload
            .get("status_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("quote authorization lacks status_id".into()))?;
        let authorization_uri = payload
            .get("authorization_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Validation("quote authorization lacks authorization_uri".into())
            })?;
        let Some(status) = posts::load(&ctx.db, status_id).await? else {
            return Ok(());
        };
        if !status.local || status.tombstone {
            return Ok(());
        }
        // The Accept was matched to this quote at ingest; re-check before
        // stamping so an edit can't inherit an approval for another target.
        let expected = payload
            .get("quoted_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Validation("quote authorization lacks quoted_uri".into()))?;
        if posts::quote_target_uri(&ctx.db, &status).await?.as_deref() != Some(expected) {
            return Ok(());
        }
        sqlx::query(
            "UPDATE statuses SET quote_authorization_uri = $2, updated_at = now() \
             WHERE id = $1 \
               AND (quote_authorization_uri IS NULL OR quote_authorization_uri <> $2)",
        )
        .bind(status_id)
        .bind(authorization_uri)
        .execute(&ctx.db)
        .await?;
        ap_posts::invalidate_object(&self.ap, status_id);
        let Some(status) = posts::load(&ctx.db, status_id).await? else {
            return Ok(());
        };
        let post_ctx = PostContext::from_job(ctx, Some(self.ap.clone()));
        ap_posts::publish_update(&self.ap, &post_ctx, &status, &[]).await?;
        Ok(())
    }
}

/// Register the quote-approval job (federation-enabled workers only).
pub fn register(registry: &mut Registry, ap: &ApContext) {
    registry.register(
        JobKind::QuoteAuthorization,
        QuoteAuthorizationJob::new(ap.clone()),
    );
}

/// The statuses quoting `target`, newest first (cursor = the quote
/// row id), filtered to what the viewer can see. Returns the visible
/// rows plus the raw page cursors — visibility pruning may shorten a
/// page, so the `next` link must move past pruned rows, not the last
/// visible one.
pub async fn quoted_by(
    ctx: &PostContext,
    target: &StatusRow,
    page: &crate::http::paginate::Page,
    viewer: Option<i64>,
) -> Result<(Vec<StatusRow>, Vec<i64>)> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM statuses \
         WHERE quote_of_id = $1 AND NOT tombstone \
           AND ($2::bigint IS NULL OR id < $2) \
           AND ($3::bigint IS NULL OR id > $3) \
         ORDER BY id DESC LIMIT $4",
    )
    .bind(target.id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(&ctx.db)
    .await?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(s) = posts::load(&ctx.db, *id).await? {
            rows.push(s);
        }
    }
    let visible = posts::visible_ids(&ctx.db, &rows, viewer).await?;
    rows.retain(|s| visible.contains(&s.id));
    Ok((rows, ids))
}

/// Bump `quotes_count` on the quoted target (negative `delta`
/// decrements). Works on a pool or an open transaction.
pub async fn bump_quote_counter<'e, E>(db: E, status_id: i64, delta: i64) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query("UPDATE statuses SET quotes_count = GREATEST(quotes_count + $2, 0) WHERE id = $1")
        .bind(status_id)
        .bind(delta)
        .execute(db)
        .await?;
    Ok(())
}

/// Resolve the quote target for a publish and append the Pleroma
/// `RE:` link to the rendered HTML — shared by the immediate
/// (`create::publish`) and scheduled (`scheduled::publish_one`)
/// paths. A target that vanished (tombstoned/deleted) between
/// validation and publish drops the quote entirely: `*quote_of_id`
/// is cleared so no dangling reference or counter bump survives.
pub async fn prepare_quote(
    ctx: &PostContext,
    quote_of_id: &mut Option<i64>,
    rendered_html: &mut String,
) -> Result<Option<StatusRow>> {
    let Some(qid) = *quote_of_id else {
        return Ok(None);
    };
    let Some(target) = posts::load(&ctx.db, qid).await? else {
        *quote_of_id = None;
        return Ok(None);
    };
    let url = target
        .url
        .clone()
        .or_else(|| target.uri.clone())
        .unwrap_or_default();
    if !url.is_empty() {
        rendered_html.push_str(&re_link_html(&url));
    }
    Ok(Some(target))
}

/// Post-insert quote fan-out — the FEP-044f `QuoteRequest` to a
/// remote quoted author (never blocking on the answer) and the
/// `quote` notification event. Best-effort; shared by both publish
/// paths.
pub async fn finish_quote(
    ctx: &PostContext,
    ap: Option<&ApContext>,
    status: &StatusRow,
    target: &StatusRow,
) {
    if let Some(ap) = ap {
        if let Err(e) = send_quote_request(ctx, ap, status, target).await {
            tracing::warn!(status_id = status.id, error = %e, "AP QuoteRequest failed");
        }
    }
    emit_notification_event(
        &ctx.db,
        &ctx.dispatcher,
        "quote",
        status.account_id,
        Some(target.id),
        target.account_id,
    )
    .await;
}
