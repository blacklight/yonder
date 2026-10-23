//! Follow requests — the pending-relationship queue every locked
//! account keeps (`follow_requests` rows), plus authorize/reject.
//!
//! Local requesters promote in SQL; remote requesters go through
//! pubby's [`pubby_sdk::follows`] helpers, which promote the stored
//! `FollowRequest` to a `follows` row via the storage adapter and hand
//! the `Accept`/`Reject` to our delivery ledger. The `activities` row
//! must exist before `deliver_fn` enqueues — [`response_options`]
//! stores it first.

use std::sync::Arc;

use pubby_sdk::follows::FollowResponseOptions;
use pubby_sdk::model::FollowRequest;
use pubby_sdk::ActivityPubStorage;
use serde_json::Value;
use sqlx::{PgPool, Row};

use crate::{
    activitypub::{actors::sync_call, delivery},
    http::{paginate::Page, AppState},
    interactions::emit_notification_event,
    Error, Result,
};

/// Pending requesters of `target_id`, newest first — `(account_id,
/// cursor)` where the cursor is `follow_requests.id`.
pub async fn list(db: &PgPool, target_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query(
        "SELECT r.account_id, r.id AS cursor FROM follow_requests r \
         JOIN accounts a ON a.id = r.account_id \
         WHERE r.target_account_id = $1 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
           AND ($2::bigint IS NULL OR r.id < $2) \
           AND ($3::bigint IS NULL OR r.id > $3) \
         ORDER BY r.id DESC LIMIT $4",
    )
    .bind(target_id)
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

/// Approve `requester_id`'s pending request on `target_id` (the
/// caller). Local requesters promote directly; remote requesters
/// promote through pubby (which writes the `follows` row itself) and
/// deliver an `Accept`.
pub async fn authorize(state: &AppState, target_id: i64, requester_id: i64) -> Result<()> {
    let (requester_actor, requester_remote) = pending(state, target_id, requester_id).await?;
    let promoted = if requester_remote {
        let (core, target_actor) = require_ap(state, target_id).await?;
        let storage = core.storage_trait();
        let request = fetch_request(&core, &requester_actor, &target_actor)?;
        let options = response_options(&core);
        sync_call(move || {
            pubby_sdk::follows::accept_follow_request(
                storage.as_ref(),
                &request,
                &target_actor,
                &options,
                Some(&core.doc_cache),
            )
        })
        .map_err(crate::Error::Federation)?;
        true
    } else {
        // Local requester on a locked local target — promote in SQL.
        // The follows trigger owns the counters.
        let mut tx = state.db.begin().await?;
        sqlx::query("DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2")
            .bind(requester_id)
            .bind(target_id)
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query(
            "INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2) \
             ON CONFLICT (account_id, target_account_id) DO NOTHING",
        )
        .bind(requester_id)
        .bind(target_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        inserted
    };
    if promoted {
        emit_notification_event(
            &state.db,
            &state.dispatcher,
            "follow",
            requester_id,
            None,
            target_id,
        )
        .await;
    }
    Ok(())
}

/// Reject `requester_id`'s pending request on `target_id`.
pub async fn reject(state: &AppState, target_id: i64, requester_id: i64) -> Result<()> {
    let (requester_actor, requester_remote) = pending(state, target_id, requester_id).await?;
    if requester_remote {
        let (core, target_actor) = require_ap(state, target_id).await?;
        let storage = core.storage_trait();
        let request = fetch_request(&core, &requester_actor, &target_actor)?;
        let options = response_options(&core);
        sync_call(move || {
            pubby_sdk::follows::reject_follow_request(
                storage.as_ref(),
                &request,
                &target_actor,
                &options,
            )
        })
        .map_err(crate::Error::Federation)?;
    } else {
        sqlx::query("DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2")
            .bind(requester_id)
            .bind(target_id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// The pending row must exist and the requester must still be a live
/// account — returns `(requester actor_id, is_remote)`.
async fn pending(state: &AppState, target_id: i64, requester_id: i64) -> Result<(String, bool)> {
    let row = sqlx::query(
        "SELECT a.actor_id, a.domain IS NOT NULL AS remote \
         FROM follow_requests r JOIN accounts a ON a.id = r.account_id \
         WHERE r.account_id = $1 AND r.target_account_id = $2 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL",
    )
    .bind(requester_id)
    .bind(target_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(Error::NotFound("follow request".into()));
    };
    Ok((row.get("actor_id"), row.get("remote")))
}

/// The federation core plus the target's `actor_id` — remote
/// authorize/reject requires both.
async fn require_ap(
    state: &AppState,
    target_id: i64,
) -> Result<(Arc<crate::activitypub::ApCore>, String)> {
    let ap = state
        .ap
        .as_ref()
        .ok_or_else(|| Error::Internal("federation is disabled".to_string()))?;
    let actor_id: String =
        sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NULL")
            .bind(target_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| Error::NotFound("account".into()))?;
    Ok((ap.core.clone(), actor_id))
}

/// Load the stored `FollowRequest` through the storage adapter (the
/// same view pubby wrote it with). 404s when the row vanished between
/// the pending check and this fetch.
fn fetch_request(
    core: &Arc<crate::activitypub::ApCore>,
    requester_actor_id: &str,
    target_actor_id: &str,
) -> Result<FollowRequest> {
    let storage = core.storage.clone();
    let requester = requester_actor_id.to_string();
    let target = target_actor_id.to_string();
    sync_call(move || storage.get_follow_request(&requester, &target))
        .map_err(crate::Error::Federation)?
        .ok_or_else(|| Error::NotFound("follow request".into()))
}

/// `FollowResponseOptions` whose `deliver` stores the `Accept`/`Reject`
/// in `activities` before the ledger enqueue — [`delivery::deliver_fn`]
/// only delivers activities it can join to a stored row.
fn response_options(core: &Arc<crate::activitypub::ApCore>) -> FollowResponseOptions {
    let storage = core.storage_trait();
    let deliver = delivery::deliver_fn(core);
    let wrapped: pubby_sdk::handlers::DeliverFn = Arc::new(move |inbox: &str, activity: &Value| {
        if let Some(id) = activity
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if let Err(e) = storage.store_activity(id, activity) {
                tracing::warn!(error = %e, "follow response activity store failed");
            }
        }
        deliver(inbox, activity);
    });
    FollowResponseOptions {
        deliver: Some(wrapped),
        ..Default::default()
    }
}
