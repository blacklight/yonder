//! Notifications (epic 16) — generation, read surface and live push.
//!
//! Generation is two-stage: emit sites ([`emit`],
//! `interactions::emit_notification_event`, `notify_report`) enqueue a
//! typed `NotificationFanout` payload; the job handler in [`fanout`]
//! re-checks suppression (blocks both ways, `hide_notifications`
//! mutes, domain blocks, `block_stranger_notifications`) and inserts
//! the row behind the `ix_notifications_dedupe` expression index —
//! at-least-once job delivery collapses to one row.
//!
//! Live push is single-path: producers always publish through Redis
//! pub/sub ([`publish_live`]) and each serving process relays
//! `{prefix}:stream` onto its local `EventBus`
//! ([`crate::http::ws::relay`]) — a standalone worker's notifications
//! reach WebSocket clients, and in-process fan-out can't
//! double-deliver.
//!
//! Invariants:
//!
//! - **Postgres is the source of truth.** Pub/sub carries events only;
//!   a dropped message never loses the row.
//! - **Suppression is run-time.** Emit sites drop the obvious cases;
//!   the fanout handler re-evaluates everything because state may have
//!   changed between enqueue and run.
//! - **Owner-scoped reads.** Every query keys on the recipient's
//!   `account_id` — foreign ids are 404, never leaked.

pub mod emit;
pub mod fanout;
pub mod jobs;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{
    cache::Redis, http::paginate::Page, http::ws::StreamEvent, jobs::Dispatcher,
    posts::PostContext, Error, Result,
};

/// A notification type — one entry per `notifications.type` CHECK value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationType {
    Follow,
    FollowRequest,
    Mention,
    Favourite,
    Reblog,
    Quote,
    Poll,
    Status,
    Update,
    EmojiReaction,
    AdminSignUp,
    ModerationWarning,
    AdminReport,
}

impl NotificationType {
    /// The `notifications.type` / Mastodon wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::FollowRequest => "follow_request",
            Self::Mention => "mention",
            Self::Favourite => "favourite",
            Self::Reblog => "reblog",
            Self::Quote => "quote",
            Self::Poll => "poll",
            Self::Status => "status",
            Self::Update => "update",
            Self::EmojiReaction => "emoji_reaction",
            Self::AdminSignUp => "admin.sign_up",
            Self::ModerationWarning => "moderation_warning",
            Self::AdminReport => "admin.report",
        }
    }

    /// Wire value → enum (`None` for unknown strings).
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "follow" => Self::Follow,
            "follow_request" => Self::FollowRequest,
            "mention" => Self::Mention,
            "favourite" => Self::Favourite,
            "reblog" => Self::Reblog,
            "quote" => Self::Quote,
            "poll" => Self::Poll,
            "status" => Self::Status,
            "update" => Self::Update,
            "emoji_reaction" => Self::EmojiReaction,
            "admin.sign_up" => Self::AdminSignUp,
            "moderation_warning" => Self::ModerationWarning,
            "admin.report" => Self::AdminReport,
            _ => return None,
        })
    }

    /// The `users.email_notif_*` column gating email for this type, if
    /// any (reactions, `status` and `admin.report` send no email).
    pub fn email_flag(&self) -> Option<&'static str> {
        Some(match self {
            Self::Follow => "email_notif_follow",
            Self::FollowRequest => "email_notif_follow_request",
            Self::Reblog => "email_notif_reblog",
            Self::Favourite => "email_notif_favourite",
            Self::Mention => "email_notif_mention",
            Self::Quote => "email_notif_quote",
            _ => return None,
        })
    }
}

/// A `notifications` row.
#[derive(Debug, Clone)]
pub struct NotificationRow {
    pub id: i64,
    /// Recipient (`accounts.id` — always local in practice).
    pub account_id: i64,
    /// The account that triggered the notification.
    pub from_account_id: i64,
    pub kind: NotificationType,
    pub status_id: Option<i64>,
    pub report_id: Option<i64>,
    pub read: bool,
    pub created_at: DateTime<Utc>,
}

fn row_to_notification(r: &sqlx::postgres::PgRow) -> NotificationRow {
    let raw: String = r.get("type");
    NotificationRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        from_account_id: r.get("from_account_id"),
        // The CHECK guarantees a known value; fall back to a neutral
        // type rather than failing the whole page if one ever slips in.
        kind: NotificationType::from_wire(&raw).unwrap_or_else(|| {
            tracing::warn!(kind = %raw, "unknown notification type");
            NotificationType::Status
        }),
        status_id: r.get("status_id"),
        report_id: r.get("report_id"),
        read: r.get("read"),
        created_at: r.get("created_at"),
    }
}

const NOTIFICATION_COLS: &str =
    "id, account_id, from_account_id, type, status_id, report_id, read, created_at";

/// The notification's embedded status must be one the recipient
/// (`$1`) can see — `posts::can_see` inlined so `list`/`unread_count`
/// drop stray rows (fan-out predating the visibility gate, or a
/// mention an edit revoked) instead of leaking them into pages and
/// the unread badge. `status_id IS NULL` rows (follows, reports)
/// always pass.
fn status_visible(alias: &str) -> String {
    format!(
        "({alias}.status_id IS NULL OR EXISTS ( \
         SELECT 1 FROM statuses vs WHERE vs.id = {alias}.status_id AND ( \
             vs.account_id = $1 OR vs.visibility IN ('public', 'unlisted') \
             OR EXISTS (SELECT 1 FROM status_mentions vsm \
                        WHERE vsm.status_id = vs.id AND vsm.account_id = $1) \
             OR (vs.visibility IN ('followers_only', 'private') \
                 AND EXISTS (SELECT 1 FROM follows vf \
                             WHERE vf.account_id = $1 \
                               AND vf.target_account_id = vs.account_id)))))"
    )
}

/// Idempotent insert — `Ok(None)` when the event already exists.
///
/// The arbiter columns must mirror `ix_notifications_dedupe`,
/// including the `COALESCE` expression.
pub async fn insert(
    db: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: NotificationType,
    status_id: Option<i64>,
    report_id: Option<i64>,
) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "INSERT INTO notifications (account_id, from_account_id, type, status_id, report_id) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (account_id, from_account_id, type, COALESCE(status_id, 0)) DO NOTHING \
         RETURNING id",
    )
    .bind(account_id)
    .bind(from_account_id)
    .bind(kind.as_str())
    .bind(status_id)
    .bind(report_id)
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// One notification owned by `account_id`.
pub async fn get(db: &PgPool, account_id: i64, id: i64) -> Result<Option<NotificationRow>> {
    sqlx::query(&format!(
        "SELECT {NOTIFICATION_COLS} FROM notifications WHERE account_id = $1 AND id = $2"
    ))
    .bind(account_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .map(|r| r.as_ref().map(row_to_notification))
    .map_err(Into::into)
}

/// Newest-first page owned by `account_id`. `types`/`exclude_types`
/// are wire type names (already alias-expanded by the caller); an
/// empty `types` means no inclusion filter. Unknown names simply
/// match no rows — Mastodon's semantics for type filters.
pub async fn list(
    db: &PgPool,
    account_id: i64,
    types: &[String],
    exclude_types: &[String],
    page: &Page,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Result<Vec<NotificationRow>> {
    // Epic-20 hide rules apply to the *embedded* status. Notifications
    // without one (follows, admin, reports) always survive.
    let mut hide_sql = String::new();
    let mut hide_binds = Vec::new();
    if let Some(hide) = hide {
        let (inner, hb) = hide.numbered_inner(6);
        hide_binds = hb;
        hide_sql = format!(
            " AND (status_id IS NULL OR NOT EXISTS (SELECT 1 FROM statuses s \
             WHERE s.id = notifications.status_id AND ({inner})))"
        );
    }
    let sql = format!(
        "SELECT {} FROM notifications \
         WHERE account_id = $1 \
           AND ($2::bigint IS NULL OR id < $2) \
           AND ($3::bigint IS NULL OR id > $3) \
           AND (cardinality($4) = 0 OR type = ANY($4)) \
           AND (cardinality($5) = 0 OR type <> ALL($5)) \
           AND {}{} \
         ORDER BY id DESC LIMIT $6",
        NOTIFICATION_COLS,
        status_visible("notifications"),
        hide_sql
    );
    let mut q = sqlx::query(&sql)
        .bind(account_id)
        .bind(page.max_id)
        .bind(page.since_id)
        .bind(types)
        .bind(exclude_types)
        .bind(page.limit);
    for b in &hide_binds {
        q = match b {
            crate::timelines::filter::Bind::I64(v) => q.bind(*v),
            crate::timelines::filter::Bind::I64s(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Text(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Texts(v) => q.bind(v.clone()),
        };
    }
    q.fetch_all(db)
        .await
        .map(|rows| rows.iter().map(row_to_notification).collect())
        .map_err(Into::into)
}

/// Does the notification's embedded status match a `hide` rule —
/// the single-row mirror of [`list`]'s `NOT EXISTS` predicate?
/// `false` when the row has no status or no `hide` clause (the
/// common case, zero extra SQL).
pub async fn hidden(
    db: &PgPool,
    row: &NotificationRow,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Result<bool> {
    let (Some(status_id), Some(hide)) = (row.status_id, hide) else {
        return Ok(false);
    };
    let (inner, binds) = hide.numbered_inner(1);
    let sql = format!("SELECT EXISTS (SELECT 1 FROM statuses s WHERE s.id = $1 AND ({inner}))");
    let mut q = sqlx::query_scalar::<_, bool>(&sql).bind(status_id);
    for b in &binds {
        q = match b {
            crate::timelines::filter::Bind::I64(v) => q.bind(*v),
            crate::timelines::filter::Bind::I64s(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Text(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Texts(v) => q.bind(v.clone()),
        };
    }
    q.fetch_one(db).await.map_err(Into::into)
}

/// Unread rows for the unread-count endpoint and live badge updates.
/// `hide` (the same clause [`list`] applies) excludes rows whose
/// status a `hide` rule would strip — the badge then matches the
/// list the client will render.
pub async fn unread_count(
    db: &PgPool,
    account_id: i64,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Result<i64> {
    let mut sql = format!(
        "SELECT COUNT(*) FROM notifications n \
         WHERE n.account_id = $1 AND NOT n.read AND {}",
        status_visible("n")
    );
    let binds = match hide {
        Some(h) => {
            let (inner, binds) = h.numbered_inner(1);
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM statuses s \
                 WHERE s.id = n.status_id AND ({inner}))"
            ));
            binds
        }
        None => Vec::new(),
    };
    let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(account_id);
    for b in &binds {
        q = match b {
            crate::timelines::filter::Bind::I64(v) => q.bind(*v),
            crate::timelines::filter::Bind::I64s(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Text(v) => q.bind(v.clone()),
            crate::timelines::filter::Bind::Texts(v) => q.bind(v.clone()),
        };
    }
    q.fetch_one(db).await.map_err(Into::into)
}

/// Delete every notification owned by `account_id`.
pub async fn clear(db: &PgPool, account_id: i64) -> Result<u64> {
    sqlx::query("DELETE FROM notifications WHERE account_id = $1")
        .bind(account_id)
        .execute(db)
        .await
        .map(|r| r.rows_affected())
        .map_err(Into::into)
}

/// Mark the caller's notifications read in bulk (viewport-driven —
/// the SPA posts the ids it rendered). Already-read rows are a no-op;
/// returns how many rows flipped.
pub async fn mark_read(db: &PgPool, account_id: i64, ids: &[i64]) -> Result<u64> {
    sqlx::query(
        "UPDATE notifications SET read = true \
         WHERE account_id = $1 AND id = ANY($2) AND NOT read",
    )
    .bind(account_id)
    .bind(ids)
    .execute(db)
    .await
    .map(|r| r.rows_affected())
    .map_err(Into::into)
}

/// Delete one notification owned by `account_id`. `false` when the row
/// does not exist or belongs to someone else.
pub async fn dismiss(db: &PgPool, account_id: i64, id: i64) -> Result<bool> {
    sqlx::query("DELETE FROM notifications WHERE account_id = $1 AND id = $2")
        .bind(account_id)
        .bind(id)
        .execute(db)
        .await
        .map(|r| r.rows_affected() > 0)
        .map_err(Into::into)
}

/// The Mastodon `Notification` entity for a row — the single-row
/// form of [`entities`]. 404s when the embedded status is one the
/// recipient can't see (the page drops such rows entirely).
pub async fn entity(
    ctx: &PostContext,
    row: &NotificationRow,
    rules: &[crate::filters::Rule],
) -> Result<Value> {
    entities(ctx, std::slice::from_ref(row), rules)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound("notification".into()))
}

/// [`entity`] for a page of notifications. Every per-row lookup the
/// naive loop made — actor account, embedded status (+ its full
/// render), report, filtered attach — is batched across the page:
/// ~20 rows cost a handful of `try_join!` waves instead of ~10
/// sequential queries each. Missing referenced objects degrade to
/// absent/`null` fields rather than failing the page — a deleted
/// status or purged actor should not 500 a notifications page.
pub async fn entities(
    ctx: &PostContext,
    rows: &[NotificationRow],
    rules: &[crate::filters::Rule],
) -> Result<Vec<Value>> {
    use std::collections::{BTreeMap, HashMap, HashSet};
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let base_url = ctx.base_url();

    // Embedded statuses rendered with the notification's owner as
    // viewer — a page is single-owner in practice, but group anyway
    // so the function stays correct for arbitrary row sets.
    let mut by_owner: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for r in rows {
        if let Some(sid) = r.status_id {
            let ids = by_owner.entry(r.account_id).or_default();
            if !ids.contains(&sid) {
                ids.push(sid);
            }
        }
    }
    let mut status_entities: HashMap<i64, Value> = HashMap::new();
    // A notification must never surface a status its recipient can't
    // see (a stray `status` row from before visibility-gated fan-out,
    // or a mention an edit revoked). The whole notification drops out —
    // even its existence would leak that the author posted.
    let mut invisible: HashSet<(i64, i64)> = HashSet::new();
    for (owner, ids) in by_owner {
        let statuses = crate::posts::load_many(&ctx.db, &ids).await?;
        let status_rows: Vec<crate::posts::StatusRow> = ids
            .iter()
            .filter_map(|id| statuses.get(id))
            .cloned()
            .collect();
        let visible = crate::posts::visible_ids(&ctx.db, &status_rows, Some(owner)).await?;
        let status_rows: Vec<crate::posts::StatusRow> = status_rows
            .into_iter()
            .filter(|s| {
                let ok = visible.contains(&s.id);
                if !ok {
                    invisible.insert((owner, s.id));
                }
                ok
            })
            .collect();
        match crate::posts::entity::status_entities(ctx, &status_rows, Some(owner)).await {
            Ok(mut ents) => {
                crate::filters::apply::attach_filtered(&ctx.db, rules, &status_rows, &mut ents)
                    .await
                    .unwrap_or_else(|e| tracing::warn!(error = %e, "filtered attach failed"));
                for (row, e) in status_rows.iter().zip(ents) {
                    status_entities.insert(row.id, e);
                }
            }
            Err(e) => {
                // The old per-row render degraded failures to
                // `"status": null` — keep the page alive the same way.
                tracing::warn!(error = %e, "notification status render failed");
                for row in &status_rows {
                    status_entities.insert(row.id, Value::Null);
                }
            }
        }
    }

    // Reports and every account the page references, in one batch.
    let mut report_ids: Vec<i64> = rows.iter().filter_map(|r| r.report_id).collect();
    report_ids.sort_unstable();
    report_ids.dedup();
    let reports = if report_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query(
            "SELECT id, account_id, target_account_id, status_id, category, comment, forward, \
                    action_taken_at, assigned_account_id, created_at \
             FROM reports WHERE id = ANY($1)",
        )
        .bind(&report_ids)
        .fetch_all(&ctx.db)
        .await?
    };
    let mut account_ids: Vec<i64> = rows.iter().map(|r| r.from_account_id).collect();
    for rep in &reports {
        account_ids.push(rep.get("target_account_id"));
        if let Some(a) = rep.get::<Option<i64>, _>("assigned_account_id") {
            account_ids.push(a);
        }
    }
    account_ids.sort_unstable();
    account_ids.dedup();
    let accounts = crate::users::account::load_many_incl_suspended(&ctx.db, &account_ids).await?;
    let account_values: HashMap<i64, Value> =
        match crate::users::account::account_entities(&ctx.db, &ctx.config, &base_url, &accounts)
            .await
        {
            Ok(ents) => accounts.iter().map(|a| a.id).zip(ents).collect(),
            // The batch emoji/moved pass failing shouldn't blank the
            // page — fall back to per-account renders, which degrade
            // individually to `null` exactly like the old loop did.
            Err(e) => {
                tracing::warn!(error = %e, "notification account batch render failed");
                let mut map = HashMap::new();
                for a in &accounts {
                    let v =
                        crate::users::account::account_entity(&ctx.db, &ctx.config, &base_url, a)
                            .await
                            .unwrap_or_else(|e| {
                                tracing::warn!(error = %e, "notification account render failed");
                                Value::Null
                            });
                    map.insert(a.id, v);
                }
                map
            }
        };

    let mut report_values: HashMap<i64, Value> = HashMap::new();
    for rep in &reports {
        let rid: i64 = rep.get("id");
        let target = account_values
            .get(&rep.get::<i64, _>("target_account_id"))
            .cloned()
            .unwrap_or(Value::Null);
        let assigned = rep
            .get::<Option<i64>, _>("assigned_account_id")
            .and_then(|id| account_values.get(&id).cloned())
            .unwrap_or(Value::Null);
        let status_id: Option<i64> = rep.get("status_id");
        let taken: Option<DateTime<Utc>> = rep.get("action_taken_at");
        report_values.insert(
            rid,
            json!({
                "id": rid.to_string(),
                "action_taken": taken.is_some(),
                "action_taken_at": taken.map(|t| t.to_rfc3339()),
                "category": rep.get::<String, _>("category"),
                "comment": rep.get::<String, _>("comment"),
                "forwarded": rep.get::<bool, _>("forward"),
                "created_at": rep.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
                "account_id": rep.get::<i64, _>("account_id").to_string(),
                "target_account": target,
                "assigned_account": assigned,
                "status_ids": status_id.map(|id| vec![id.to_string()]).unwrap_or_default(),
                "rule_ids": [],
            }),
        );
    }

    Ok(rows
        .iter()
        .filter_map(|row| {
            if row
                .status_id
                .is_some_and(|sid| invisible.contains(&(row.account_id, sid)))
            {
                return None;
            }
            let mut out = json!({
                "id": row.id.to_string(),
                "type": row.kind.as_str(),
                "created_at": row.created_at.to_rfc3339(),
                "read": row.read,
                "account": account_values
                    .get(&row.from_account_id)
                    .cloned()
                    .unwrap_or(Value::Null),
            });
            if let Some(status_id) = row.status_id {
                if let Some(rendered) = status_entities.get(&status_id) {
                    out["status"] = rendered.clone();
                }
            }
            if let Some(report_id) = row.report_id {
                out["report"] = report_values
                    .get(&report_id)
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            Some(out)
        })
        .collect())
}

/// Publish a `StreamEvent` through the Redis relay channel — the
/// single live-push path (see the module doc). Best-effort: failures
/// log a warning, never error the caller.
pub async fn publish_live(redis: &Redis, stream: &str, event: &str, payload: &Value) {
    let ev = StreamEvent {
        stream: stream.to_string(),
        event: event.to_string(),
        payload: payload.clone(),
    };
    if let Err(e) = redis.publish(&redis.key("stream"), &json!(ev)).await {
        tracing::warn!(stream, event, error = %e, "live publish failed");
    }
}

/// `admin.report` — one notification event per local
/// admin/moderator, excluding the reporter. Epic 21 wires the caller
/// (`POST /api/v1/reports`); a missing report row is a 404.
pub async fn notify_report(db: &PgPool, dispatcher: &Dispatcher, report_id: i64) -> Result<()> {
    let Some((reporter, status_id)) = sqlx::query_as::<_, (i64, Option<i64>)>(
        "SELECT account_id, status_id FROM reports WHERE id = $1",
    )
    .bind(report_id)
    .fetch_optional(db)
    .await?
    else {
        return Err(Error::NotFound("report".into()));
    };
    let recipients: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE domain IS NULL AND role IN ('admin', 'moderator') \
           AND suspended_at IS NULL AND deleted_at IS NULL AND id <> $1",
    )
    .bind(reporter)
    .fetch_all(db)
    .await?;
    for to in recipients {
        crate::interactions::emit_notification_event_ext(
            db,
            dispatcher,
            "admin.report",
            reporter,
            status_id,
            Some(report_id),
            to,
        )
        .await;
    }
    Ok(())
}

/// Register epic-16 job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    jobs::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_type_round_trip() {
        for kind in [
            NotificationType::Follow,
            NotificationType::FollowRequest,
            NotificationType::Mention,
            NotificationType::Favourite,
            NotificationType::Reblog,
            NotificationType::Quote,
            NotificationType::Poll,
            NotificationType::Status,
            NotificationType::Update,
            NotificationType::EmojiReaction,
            NotificationType::AdminSignUp,
            NotificationType::ModerationWarning,
            NotificationType::AdminReport,
        ] {
            assert_eq!(NotificationType::from_wire(kind.as_str()), Some(kind));
        }
        assert_eq!(NotificationType::from_wire("bogus"), None);
        assert_eq!(
            NotificationType::Favourite.email_flag(),
            Some("email_notif_favourite")
        );
        assert_eq!(NotificationType::AdminReport.email_flag(), None);
    }
}
