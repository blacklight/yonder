//! Follow/unfollow REST-facing services — thin wrappers over
//! [`crate::users::relationships`] that add the notification events
//! and the paginated follower/following listings.

use sqlx::{PgPool, Row};

use crate::{
    http::{paginate::Page, AppState},
    interactions::emit_notification_event,
    users::relationships::{self, Ctx},
    Error, Result,
};

use super::{FollowOpts, FollowOutcome};

/// Follow `target_id` on behalf of `follower_id`, emitting the
/// `follow`/`follow_request` notification event for local targets
/// ([`emit_notification_event`] drops remote recipients itself —
/// a remote follow is announced to the remote by the AP activity,
/// not by our notification pipeline).
pub async fn follow(
    state: &AppState,
    follower_id: i64,
    target_id: i64,
    opts: FollowOpts,
) -> Result<FollowOutcome> {
    let ctx = Ctx::from_state(state);
    let outcome = relationships::follow(ctx, follower_id, target_id, opts).await?;
    let kind = match outcome {
        FollowOutcome::Following => Some("follow"),
        FollowOutcome::Requested => Some("follow_request"),
    };
    if let Some(kind) = kind {
        emit_notification_event(
            &state.db,
            &state.dispatcher,
            kind,
            follower_id,
            None,
            target_id,
        )
        .await;
    }
    if matches!(outcome, FollowOutcome::Following) {
        // A landed follow seeds the follower's home index with the
        // followee's recent statuses. A `Requested` remote follow
        // backfills later, off the inbound `Accept`.
        crate::timelines::fanout::enqueue_backfill(&state.dispatcher, follower_id, target_id).await;
    }
    Ok(outcome)
}

/// Unfollow `target_id`; returns whether a row was removed.
pub async fn unfollow(state: &AppState, follower_id: i64, target_id: i64) -> Result<bool> {
    let ctx = Ctx::from_state(state);
    relationships::unfollow(ctx, follower_id, target_id).await
}

/// `POST /api/v1/follows` (deprecated): resolve `uri` — a
/// `user@domain` handle, bare local username or actor URL — then
/// follow. Returns the target's `accounts.id`.
pub async fn follow_by_uri(state: &AppState, follower_id: i64, uri: &str) -> Result<i64> {
    let ctx = Ctx::from_state(state);
    let target_id = relationships::resolve_account(ctx, uri).await?;
    follow(state, follower_id, target_id, FollowOpts::default()).await?;
    Ok(target_id)
}

/// `(account_id, cursor)` pages for followers/following lists —
/// newest `follows` rows first, cursor is `follows.id`.
async fn edge_page(
    db: &PgPool,
    direction: &str,
    account_id: i64,
    page: &Page,
) -> Result<Vec<(i64, i64)>> {
    // $1 = the listed account; cursor/filter params follow.
    let query = format!(
        "SELECT a.id AS account_id, f.id AS cursor FROM follows f \
         JOIN accounts a ON a.id = f.{direction} \
         WHERE f.{other} = $1 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
           AND ($2::bigint IS NULL OR f.id < $2) \
           AND ($3::bigint IS NULL OR f.id > $3) \
         ORDER BY f.id DESC LIMIT $4",
        direction = direction,
        other = other_column(direction),
    );
    let rows = sqlx::query(&query)
        .bind(account_id)
        .bind(page.max_id)
        .bind(page.since_id)
        .bind(page.limit)
        .fetch_all(db)
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("account_id"), r.get("cursor")))
        .collect())
}

fn other_column(direction: &str) -> &'static str {
    match direction {
        "account_id" => "target_account_id",
        "target_account_id" => "account_id",
        _ => unreachable!("direction is a fixed literal"),
    }
}

/// Accounts following `target_id` (its followers).
pub async fn followers(db: &PgPool, target_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_page(db, "account_id", target_id, page).await
}

/// Accounts `account_id` follows.
pub async fn following(db: &PgPool, account_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_page(db, "target_account_id", account_id, page).await
}

/// Whether `hide_network` keeps the caller out of `account`'s
/// follower/following listings — the owner and admins see through it.
pub async fn network_hidden(db: &PgPool, account_id: i64, viewer: Option<i64>) -> Result<bool> {
    if viewer == Some(account_id) {
        return Ok(false);
    }
    let hidden: bool =
        sqlx::query_scalar("SELECT COALESCE(hide_network, false) FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    Ok(hidden)
}

/// Load a visible target account for the `accounts/{id}` actions, or
/// 404 — nonexistent, suspended and deleted accounts are all absent
/// from relationship writes.
pub async fn visible_target(db: &PgPool, account_id: i64) -> Result<()> {
    if !super::visibility::visible(db, account_id).await? {
        return Err(Error::NotFound("account".into()));
    }
    Ok(())
}
