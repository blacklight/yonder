//! Block/mute/domain-block services on top of the epic-11 local
//! writers, plus the `Block`/`Undo(Block)` federation pubby doesn't
//! build.
//!
//! `admin.unfollow_blocked` (default true) removes the follow
//! relationships in both directions when a block lands — the same
//! semantic Mastodon applies and the import path mirrors.

use serde_json::json;
use sqlx::{PgPool, Row};

use crate::{
    activitypub::delivery,
    http::{paginate::Page, AppState},
    users::relationships::{self, Ctx},
    Error, Result,
};

/// Block `target_id` for `caller_id`: row first, then the
/// `unfollow_blocked` sever in both directions, then the outbound
/// `Block` activity for remote targets. Idempotent at every step — a
/// repeat block doesn't re-publish.
pub async fn block(state: &AppState, caller_id: i64, target_id: i64) -> Result<()> {
    if caller_id == target_id {
        return Err(Error::Validation("cannot block yourself".to_string()));
    }
    let changed = relationships::block(&state.db, caller_id, target_id).await?;

    if state.config().admin.unfollow_blocked {
        let cfg = state.config();
        let ctx = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
        relationships::unfollow(ctx, caller_id, target_id).await?;
        relationships::unfollow(ctx, target_id, caller_id).await?;
    }

    if !changed {
        return Ok(());
    }
    if let (Some(ap), Some((actor_id, username, target_actor_id))) = (
        state.ap.as_ref(),
        actor_pair(&state.db, caller_id, target_id).await?,
    ) {
        let activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor_id}#blocks/{}", uuid::Uuid::new_v4()),
            "type": "Block",
            "actor": actor_id,
            "object": target_actor_id,
            "to": [target_actor_id],
        });
        if let Err(e) = delivery::publish(ap, &username, &activity).await {
            // The local block is already correct; a failed publish is
            // audit noise, not a reason to roll it back.
            tracing::warn!(error = %e, "block activity publish failed");
        }
    }
    Ok(())
}

/// Unblock `target_id`; `Undo(Block)` goes to remote targets — only
/// when a block row actually existed (a bare unblock sends nothing).
pub async fn unblock(state: &AppState, caller_id: i64, target_id: i64) -> Result<()> {
    if !relationships::unblock(&state.db, caller_id, target_id).await? {
        return Ok(());
    }

    if let (Some(ap), Some((actor_id, username, target_actor_id))) = (
        state.ap.as_ref(),
        actor_pair(&state.db, caller_id, target_id).await?,
    ) {
        let activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor_id}#unblocks/{}", uuid::Uuid::new_v4()),
            "type": "Undo",
            "actor": actor_id,
            "object": {
                "type": "Block",
                "actor": actor_id,
                "object": target_actor_id,
            },
            "to": [target_actor_id],
        });
        if let Err(e) = delivery::publish(ap, &username, &activity).await {
            tracing::warn!(error = %e, "unblock Undo publish failed");
        }
    }
    Ok(())
}

/// `(caller actor_id, caller username, target actor_id)` for remote
/// targets only — `None` when the target is local or the row is gone.
async fn actor_pair(
    db: &PgPool,
    caller_id: i64,
    target_id: i64,
) -> Result<Option<(String, String, String)>> {
    let row = sqlx::query(
        "SELECT a.actor_id, a.username, t.actor_id AS target_actor_id \
         FROM accounts a, accounts t \
         WHERE a.id = $1 AND a.domain IS NULL AND t.id = $2 AND t.domain IS NOT NULL",
    )
    .bind(caller_id)
    .bind(target_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| {
        (
            r.get("actor_id"),
            r.get("username"),
            r.get("target_actor_id"),
        )
    }))
}

/// Mute `target_id` for `caller_id` — `duration_secs` maps to
/// `mutes.expires_at` (`None` = indefinite).
pub async fn mute(
    state: &AppState,
    caller_id: i64,
    target_id: i64,
    hide_notifications: bool,
    duration_secs: Option<i64>,
) -> Result<()> {
    relationships::mute(
        &state.db,
        caller_id,
        target_id,
        hide_notifications,
        duration_secs,
    )
    .await
}

/// Unmute `target_id` for `caller_id`.
pub async fn unmute(state: &AppState, caller_id: i64, target_id: i64) -> Result<()> {
    relationships::unmute(&state.db, caller_id, target_id).await
}

/// Accounts `caller_id` blocks — newest first, `(account_id, cursor)`
/// where the cursor is `blocks.id`.
pub async fn blocked_list(db: &PgPool, caller_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_list(
        db,
        "blocks",
        "b.id",
        "a.suspended_at IS NULL",
        caller_id,
        page,
    )
    .await
}

/// Accounts `caller_id` mutes (unexpired) — newest first.
pub async fn muted_list(db: &PgPool, caller_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_list(
        db,
        "mutes",
        "b.id",
        "b.expires_at IS NULL OR b.expires_at > now()",
        caller_id,
        page,
    )
    .await
}

/// Shared `(account_id, cursor)` pager for blocks/mutes listings.
async fn edge_list(
    db: &PgPool,
    table: &str,
    cursor_col: &str,
    extra_where: &str,
    caller_id: i64,
    page: &Page,
) -> Result<Vec<(i64, i64)>> {
    let query = format!(
        "SELECT a.id AS account_id, {cursor_col} AS cursor FROM {table} b \
         JOIN accounts a ON a.id = b.target_account_id \
         WHERE b.account_id = $1 \
           AND a.deleted_at IS NULL AND {extra_where} \
           AND ($2::bigint IS NULL OR {cursor_col} < $2) \
           AND ($3::bigint IS NULL OR {cursor_col} > $3) \
         ORDER BY {cursor_col} DESC LIMIT $4"
    );
    let rows = sqlx::query(&query)
        .bind(caller_id)
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

/// Per-user domain blocks — Mastodon's `GET/POST/DELETE
/// /api/v1/domain_blocks`.
pub async fn block_domain(state: &AppState, caller_id: i64, domain: &str) -> Result<()> {
    relationships::block_domain(&state.db, caller_id, domain).await
}

/// Remove a per-user domain block.
pub async fn unblock_domain(state: &AppState, caller_id: i64, domain: &str) -> Result<()> {
    relationships::unblock_domain(&state.db, caller_id, domain).await
}

/// The caller's blocked domains, newest first — `(domain, cursor)`.
pub async fn domain_list(db: &PgPool, caller_id: i64, page: &Page) -> Result<Vec<(String, i64)>> {
    let rows = sqlx::query(
        "SELECT d.domain, d.id AS cursor FROM account_domain_blocks d \
         WHERE d.account_id = $1 \
           AND ($2::bigint IS NULL OR d.id < $2) \
           AND ($3::bigint IS NULL OR d.id > $3) \
         ORDER BY d.id DESC LIMIT $4",
    )
    .bind(caller_id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("domain"), r.get("cursor")))
        .collect())
}
