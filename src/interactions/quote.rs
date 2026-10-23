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

use crate::{
    activitypub::{delivery, posts as ap_posts, ApContext},
    posts::{self, PostContext, StatusRow},
    Result,
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
