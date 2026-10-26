//! `NotificationFanout` — the notification generation job (epic 16).
//!
//! Two payload shapes share the kind:
//!
//! - **Local shape** (`interactions::emit_notification_event`):
//!   `{ "type", "from_account_id", "status_id"?, "report_id"?,
//!      "account_id" }` — typed ids, recipient explicit.
//! - **Remote shape** (`activitypub::incoming::on_interaction_fn`):
//!   `{ "source_actor_id", "target_resource", "interaction_type",
//!      "object_id", "activity_id" }` — pubby `InteractionType` wire
//!   values (`like`/`boost`/`quote`/`reply`/`mention`) resolved through
//!   `accounts.actor_id` and `statuses.uri`; the recipient is the
//!   resolved status' author.
//!
//! Suppression is re-checked at run time — emit sites drop the obvious
//! cases, but job delivery is at-least-once and blocks/mutes can land
//! between enqueue and run. Inserts are idempotent
//! (`ix_notifications_dedupe`), so a redelivered job is a no-op. A
//! fresh insert then publishes live events through [`publish_live`]
//! and enqueues the per-user email job when the recipient enabled it.

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{
    interactions::is_local_account,
    jobs::{Dispatcher, JobContext, JobHandler, JobKind},
    posts::PostContext,
    social::visibility,
    Error, Result,
};

use super::NotificationType;

/// The fan-out job.
pub struct NotificationFanoutJob;

/// A fully-resolved notification event.
struct Target {
    kind: NotificationType,
    from: i64,
    status_id: Option<i64>,
    report_id: Option<i64>,
    to: i64,
}

#[async_trait::async_trait]
impl JobHandler for NotificationFanoutJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(ev) = resolve(&ctx.db, payload).await? else {
            return Ok(());
        };
        if suppressed(&ctx.db, &ev).await? {
            return Ok(());
        }
        // `None` = dedupe hit: no row, no events, no email.
        let Some(id) =
            super::insert(&ctx.db, ev.to, ev.from, ev.kind, ev.status_id, ev.report_id).await?
        else {
            return Ok(());
        };
        push_live(ctx, id, &ev).await;
        enqueue_email(ctx, &ev).await;
        Ok(())
    }
}

/// Normalize either payload shape into a [`Target`]. Unknown or
/// unresolvable payloads return `None` — never retry garbage.
async fn resolve(db: &PgPool, payload: &Value) -> Result<Option<Target>> {
    if let Some(kind) = payload.get("type").and_then(Value::as_str) {
        let Some(kind) = NotificationType::from_wire(kind) else {
            tracing::warn!(kind, "notification fanout: unknown type");
            return Ok(None);
        };
        let (Some(from), Some(to)) = (
            payload.get("from_account_id").and_then(Value::as_i64),
            payload.get("account_id").and_then(Value::as_i64),
        ) else {
            return Ok(None);
        };
        return Ok(Some(Target {
            kind,
            from,
            status_id: payload.get("status_id").and_then(Value::as_i64),
            report_id: payload.get("report_id").and_then(Value::as_i64),
            to,
        }));
    }
    // Remote interaction shape — resolve URIs to local ids.
    let (Some(source), Some(resource), Some(itype)) = (
        payload.get("source_actor_id").and_then(Value::as_str),
        payload.get("target_resource").and_then(Value::as_str),
        payload.get("interaction_type").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let kind = match itype {
        "like" => NotificationType::Favourite,
        "boost" => NotificationType::Reblog,
        "quote" => NotificationType::Quote,
        // A remote reply to a local status surfaces as a mention —
        // Mastodon has no separate `reply` type.
        "reply" | "mention" => NotificationType::Mention,
        _ => return Ok(None),
    };
    let Some(from) = sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(source)
        .fetch_optional(db)
        .await?
    else {
        return Ok(None);
    };
    let Some((target_status_id, to)) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT id, account_id FROM statuses WHERE uri = $1 AND NOT tombstone",
    )
    .bind(resource)
    .fetch_optional(db)
    .await?
    else {
        // The interaction targets a remote or unknown object, or the
        // target was an actor URI — nothing local to notify.
        return Ok(None);
    };
    // `reply`/`quote` notifications key to the activity's own object
    // (the reply/quoting status), like Mastodon and like the local
    // `emit_for_status` mention — the dedupe index then collapses the
    // pair into one row instead of producing a second notification on
    // the parent. Fall back to the target when the object never
    // materialized.
    let status_id = match itype {
        "reply" | "quote" => match payload.get("object_id").and_then(Value::as_str) {
            Some(uri) => sqlx::query_scalar::<_, i64>(
                "SELECT id FROM statuses WHERE uri = $1 AND NOT tombstone",
            )
            .bind(uri)
            .fetch_optional(db)
            .await?
            .unwrap_or(target_status_id),
            None => target_status_id,
        },
        _ => target_status_id,
    };
    Ok(Some(Target {
        kind,
        from,
        status_id: Some(status_id),
        report_id: None,
        to,
    }))
}

/// Every suppression rule, re-evaluated at run time. Order is cheap
/// checks first; each returns `true` for "drop this event".
async fn suppressed(db: &PgPool, ev: &Target) -> Result<bool> {
    if ev.from == ev.to {
        return Ok(true);
    }
    match is_local_account(db, ev.to).await {
        // A recipient without an accounts row was deleted between
        // enqueue and run — suppress, don't retry.
        Err(Error::NotFound(_)) | Ok(false) => return Ok(true),
        Err(e) => return Err(e),
        Ok(true) => {}
    }
    if !visibility::visible(db, ev.from).await? || !visibility::visible(db, ev.to).await? {
        return Ok(true);
    }
    if visibility::pair_blocked(db, ev.from, ev.to).await? {
        return Ok(true);
    }
    // `hide_notifications` mutes suppress notification rows; expired
    // mutes are dead. (`visibility::muted` covers all mutes — the
    // flag-aware variant needs the extra column.)
    let muted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mutes \
         WHERE account_id = $1 AND target_account_id = $2 AND hide_notifications \
           AND (expires_at IS NULL OR expires_at > now()))",
    )
    .bind(ev.to)
    .bind(ev.from)
    .fetch_one(db)
    .await?;
    if muted {
        return Ok(true);
    }
    let sender_domain: Option<String> =
        sqlx::query_scalar("SELECT domain FROM accounts WHERE id = $1")
            .bind(ev.from)
            .fetch_optional(db)
            .await?
            .flatten();
    if visibility::domain_blocked(db, ev.to, sender_domain.as_deref()).await? {
        return Ok(true);
    }
    // "Block notifications from strangers" — a stranger is a sender the
    // recipient neither follows nor subscribes to.
    let block_strangers: bool =
        sqlx::query_scalar("SELECT block_stranger_notifications FROM users WHERE account_id = $1")
            .bind(ev.to)
            .fetch_optional(db)
            .await?
            .flatten()
            .unwrap_or(false);
    if block_strangers {
        let related: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM follows \
                            WHERE account_id = $1 AND target_account_id = $2) \
                OR EXISTS(SELECT 1 FROM subscriptions \
                          WHERE account_id = $1 AND target_account_id = $2)",
        )
        .bind(ev.to)
        .bind(ev.from)
        .fetch_one(db)
        .await?;
        if !related {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Publish the `notification` entity and the unread-count update
/// through the Redis relay. Scoped stream keys use `users.id` (see
/// `Connection::matching`); a recipient without a `users` row can't
/// have connections anyway.
async fn push_live(ctx: &JobContext, id: i64, ev: &Target) {
    let uid: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(ev.to)
        .fetch_optional(&ctx.db)
        .await
        .unwrap_or_default();
    let Some(uid) = uid else { return };
    let post_ctx = PostContext::from_job(ctx, None);
    // Fail closed: a rule-load error skips the entity publish rather
    // than pushing an unfiltered notification (the row is durable —
    // the next list/show reflects it). The unread-count update below
    // carries no filtered content, so it still goes out.
    let rules = match crate::filters::rules::load_active(
        &ctx.db,
        ev.to,
        crate::filters::FilterContext::Notifications,
    )
    .await
    {
        Ok(rules) => Some(rules),
        Err(e) => {
            tracing::warn!(notification_id = id, error = %e, "filter rules load failed");
            None
        }
    };
    let hide = rules
        .as_ref()
        .and_then(|r| crate::filters::apply::hide_clause(r));
    if let (Some(rules), Ok(Some(row))) = (&rules, super::get(&ctx.db, ev.to, id).await) {
        let hidden = match super::hidden(&ctx.db, &row, hide.as_ref()).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(notification_id = id, error = %e, "hide check failed");
                true
            }
        };
        if !hidden {
            match super::entity(&post_ctx, &row, rules).await {
                Ok(entity) => {
                    super::publish_live(
                        &ctx.redis,
                        &format!("user:notification:{uid}"),
                        "notification",
                        &entity,
                    )
                    .await;
                    super::publish_live(
                        &ctx.redis,
                        &format!("user:{uid}"),
                        "notification",
                        &entity,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!(notification_id = id, error = %e, "notification render failed")
                }
            }
        }
    }
    // Hidden rows are excluded from the badge too (when rules loaded).
    let count = super::unread_count(&ctx.db, ev.to, hide.as_ref())
        .await
        .unwrap_or(0);
    super::publish_live(
        &ctx.redis,
        &format!("user:notification:{uid}"),
        "notifications_count",
        &json!({ "unread_count": count }),
    )
    .await;
}

/// Enqueue `EmailSend` when the recipient enabled this notification
/// type in their email settings (defaults live in the column defaults,
/// migration 0002). The payload carries no addresses or secrets — the
/// sender resolves them (epic 22).
async fn enqueue_email(ctx: &JobContext, ev: &Target) {
    let Some(col) = ev.kind.email_flag() else {
        return;
    };
    let enabled: bool =
        sqlx::query_scalar(&format!("SELECT {col} FROM users WHERE account_id = $1"))
            .bind(ev.to)
            .fetch_optional(&ctx.db)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
    if !enabled {
        return;
    }
    let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    if let Err(e) = dispatcher
        .enqueue(
            JobKind::EmailSend,
            json!({
                "kind": ev.kind.as_str(),
                "account_id": ev.to,
                "from_account_id": ev.from,
                "status_id": ev.status_id,
            }),
        )
        .await
    {
        tracing::warn!(error = %e, "email notification enqueue failed");
    }
}
