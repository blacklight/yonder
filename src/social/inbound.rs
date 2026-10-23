//! Inbound arms for the `on_activity` hook: `Follow` (deferred
//! `FollowEvent` enqueue) and `Block` (audit + follow sever).
//!
//! The hook runs *before* pubby's dispatch stores anything — which is
//! why `Follow` doesn't emit its notification inline: whether pubby
//! lands a `follows` row (auto-accept) or a `follow_requests` row
//! (locked) isn't decidable here. The deferred job settles it.

use std::sync::Arc;
use std::time::Duration;

use pubby_sdk::{Activity, ActivityDisposition};
use serde_json::{json, Value};

use crate::activitypub::{actors::sync_block, ApCore};
use crate::jobs::JobKind;
use crate::Result;

/// Give pubby's dispatch time to write the row before the settle job
/// reads it. Generous on purpose — the job also no-ops cleanly when
/// the row never landed.
const FOLLOW_EVENT_DELAY: Duration = Duration::from_secs(2);

/// `Follow` — enqueue `FollowEvent` for local targets and pass
/// through. Remote-targeted Follows aren't our business (pubby ignores
/// them too). A failed enqueue is best-effort noise, not a reason to
/// drop the follow — warn and pass through (same rule as `Block`).
pub fn handle_follow(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
) -> Result<ActivityDisposition> {
    let Some(target_actor_id) = actor_uri(raw.get("object")) else {
        return Ok(ActivityDisposition::PassThrough);
    };
    let outcome = sync_block(follow_inner(core, activity.actor.clone(), target_actor_id));
    match outcome {
        Ok(()) => Ok(ActivityDisposition::PassThrough),
        Err(e) => {
            tracing::warn!(error = %e, "inbound Follow event enqueue failed; passing through");
            Ok(ActivityDisposition::PassThrough)
        }
    }
}

async fn follow_inner(
    core: &Arc<ApCore>,
    follower_actor_id: String,
    target_actor_id: String,
) -> Result<()> {
    let local: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1 AND domain IS NULL)",
    )
    .bind(&target_actor_id)
    .fetch_one(&core.db)
    .await?;
    if local {
        core.dispatcher
            .enqueue_delayed(
                JobKind::FollowEvent,
                json!({
                    "actor_id": follower_actor_id,
                    "target_actor_id": target_actor_id,
                }),
                FOLLOW_EVENT_DELAY,
            )
            .await?;
    }
    Ok(())
}

/// `Block` — keep the generic `interactions` audit row, then sever the
/// relationship between the blocked local account and the remote
/// blocker in both directions (the blocker said "don't interact with
/// me"). `Handled`, as before.
pub fn handle_block(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
) -> Result<ActivityDisposition> {
    let outcome = sync_block(block_inner(core, activity, raw));
    match outcome {
        Ok(()) => Ok(ActivityDisposition::Handled),
        Err(e) => {
            tracing::warn!(error = %e, "inbound Block handling failed; passing through");
            Ok(ActivityDisposition::PassThrough)
        }
    }
}

async fn block_inner(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<()> {
    crate::activitypub::incoming::store_unhandled(&core.storage, activity, raw).await?;
    let Some(object) = actor_uri(raw.get("object")) else {
        return Ok(());
    };
    // The local account the remote actor blocked, and the blocker's
    // local row (absent → nothing to sever).
    let pair: Option<(i64, i64)> = sqlx::query_as(
        "SELECT l.id, b.id FROM accounts l, accounts b \
         WHERE l.actor_id = $1 AND l.domain IS NULL AND b.actor_id = $2",
    )
    .bind(&object)
    .bind(&activity.actor)
    .fetch_optional(&core.db)
    .await?;
    let Some((local_id, blocker_id)) = pair else {
        return Ok(());
    };
    // A block rejects the relationship wholesale — sever follows and
    // pending requests in *both* directions (same rule
    // `unfollow_blocked` applies to outbound blocks).
    sqlx::query(
        "DELETE FROM follows \
         WHERE (account_id = $1 AND target_account_id = $2) \
            OR (account_id = $2 AND target_account_id = $1)",
    )
    .bind(local_id)
    .bind(blocker_id)
    .execute(&core.db)
    .await?;
    sqlx::query(
        "DELETE FROM follow_requests \
         WHERE (account_id = $1 AND target_account_id = $2) \
            OR (account_id = $2 AND target_account_id = $1)",
    )
    .bind(local_id)
    .bind(blocker_id)
    .execute(&core.db)
    .await?;
    Ok(())
}

/// An actor URI out of an `object`/`actor` field — either a bare URL
/// or `{"id": "…"}`.
fn actor_uri(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| {
        v.as_str()
            .or_else(|| v.get("id").and_then(|i| i.as_str()))
            .map(str::to_string)
    })
}
