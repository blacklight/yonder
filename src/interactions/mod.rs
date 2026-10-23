//! Social interactions — favourites, boosts, bookmarks, quotes, emoji
//! reactions and pins (epic 13).
//!
//! Local services live in the per-interaction modules; [`inbound`]
//! handles the matching inbound ActivityPub activities through the
//! `on_activity` hook (see `docs/interactions.md`).
//!
//! Cross-cutting invariants:
//!
//! - **Visibility**: every interaction resolves the target through
//!   [`load_visible`] — invisible targets are 404, never leak.
//! - **Idempotency**: writes rely on the UNIQUE constraints
//!   (`favourites`, `bookmarks`, `emoji_reactions`, the partial reblog
//!   index from migration 0015) with `ON CONFLICT DO NOTHING` — a
//!   repeat is a replay, not an error.
//! - **Counter consistency**: `favourites_count`/`reblogs_count`/
//!   `quotes_count` move inside the same transaction as the row write,
//!   only when `rows_affected > 0`.
//! - **Notification events, not rows**: local actions emit a typed
//!   `NotificationFanout` payload ([`emit_notification_event`]);
//!   bookmarks emit nothing; epic 16 owns the `notifications` table.
//! - **Inbound trust**: inbound handlers only touch *local* statuses
//!   and run after signature verification; remote-to-remote
//!   interactions are ignored.

pub mod bookmark;
pub mod boost;
pub mod entity;
pub mod favourite;
pub mod inbound;
pub mod pins;
pub mod quote;
pub mod reactions;

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{
    jobs::{Dispatcher, JobKind},
    posts::{self, PostContext, StatusRow},
    Error, Result,
};

/// Load a status and apply `can_see` — the invariant every interaction
/// entry point relies on. 404 on missing, tombstoned or invisible.
pub async fn load_visible(
    ctx: &PostContext,
    status_id: i64,
    viewer: Option<i64>,
) -> Result<StatusRow> {
    let status = posts::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if !posts::can_see(&ctx.db, &status, viewer).await? {
        return Err(Error::NotFound("status".into()));
    }
    Ok(status)
}

/// `username`/`actor_id` for an account (local accounts only have
/// meaningful values here).
pub async fn account_parts(ctx: &PostContext, account_id: i64) -> Result<(String, String)> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&ctx.db)
            .await?;
    row.ok_or_else(|| Error::NotFound("account".into()))
}

/// Whether `account_id` is a local account (remote rows carry a
/// `domain`).
pub async fn is_local_account(db: &PgPool, account_id: i64) -> Result<bool> {
    sqlx::query_scalar("SELECT domain IS NULL FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))
}

/// Whether `blocker` has blocked `blocked` (blocker → blocked).
pub async fn is_blocked(db: &PgPool, blocker: i64, blocked_id: i64) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(blocker)
    .bind(blocked_id)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// The stored activity JSON for a previously-published activity id
/// (`delivery::publish` persists the full payload in `activities`).
pub async fn stored_activity(db: &PgPool, activity_uri: &str) -> Result<Option<Value>> {
    sqlx::query_scalar("SELECT object FROM activities WHERE activity_id = $1 AND local")
        .bind(activity_uri)
        .fetch_optional(db)
        .await
        .map_err(Into::into)
}

/// Emit a typed `NotificationFanout` event (epic 16 consumes the job).
/// Suppressed for self-interactions, remote recipients (a remote
/// author gets the AP activity, not our notification) and blocked
/// actors. Best-effort: an enqueue failure only logs — the
/// interaction row is already durable.
///
/// `status_id` is `None` for relationship events (follow,
/// follow_request) which carry no status.
pub async fn emit_notification_event(
    db: &PgPool,
    dispatcher: &Dispatcher,
    kind: &str,
    from_account_id: i64,
    status_id: Option<i64>,
    to_account_id: i64,
) {
    emit_notification_event_ext(
        db,
        dispatcher,
        kind,
        from_account_id,
        status_id,
        None,
        to_account_id,
    )
    .await;
}

/// [`emit_notification_event`] plus a `report_id` — only
/// `admin.report` events (epic 16 `notify_report`) carry one.
pub async fn emit_notification_event_ext(
    db: &PgPool,
    dispatcher: &Dispatcher,
    kind: &str,
    from_account_id: i64,
    status_id: Option<i64>,
    report_id: Option<i64>,
    to_account_id: i64,
) {
    let outcome = async {
        if from_account_id == to_account_id {
            return Ok(());
        }
        if !is_local_account(db, to_account_id).await?
            || is_blocked(db, to_account_id, from_account_id).await?
        {
            return Ok(());
        }
        dispatcher
            .enqueue(
                JobKind::NotificationFanout,
                json!({
                    "type": kind,
                    "from_account_id": from_account_id,
                    "status_id": status_id,
                    "report_id": report_id,
                    "account_id": to_account_id,
                }),
            )
            .await
            .map(|_| ())
    };
    if let Err(e) = outcome.await {
        tracing::warn!(error = %e, kind, "notification event enqueue failed");
    }
}

/// An activity id under the actor's namespace, matching pubby's
/// `new_activity_id` shape.
pub fn new_activity_id(actor_id: &str) -> String {
    format!(
        "{}/activities/{}",
        actor_id.trim_end_matches('/'),
        uuid::Uuid::new_v4()
    )
}

/// `activity.object` → an object URI (string or `{id}` shape).
pub fn object_uri(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(m)) => m
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}
