//! Shared relationship primitive (epic 11): follow, unfollow, account
//! resolution and the local writers for blocks, mutes and domain
//! blocks. Move, archive import and auto-follow all run through here;
//! epic 14 builds the REST surface, notifications and `Block`
//! federation on top of it instead of re-implementing the handshake.
//!
//! Outbound follows are *real* ActivityPub: a remote target gets a
//! `follow_requests` row recording the `Follow` we sent (pubby's inbox
//! settles it on `Accept`/`Reject` via `on_follow_response`) plus a
//! `Follow` activity published through [`delivery::publish`], which
//! durably delivers to the target's inbox through the ledger.

use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{
    activitypub::{self, actors::sync_call, delivery, ApContext},
    config::Config,
    Error, Result,
};

/// Everything the relationship writers need — a pool, the effective
/// config and, when federation is enabled, the AP context used to
/// publish `Follow`/`Undo` and fetch remote actors.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    /// PostgreSQL pool.
    pub db: &'a PgPool,
    /// Effective configuration.
    pub config: &'a Config,
    /// Federation context — `None` when `federation.enabled` is off
    /// (remote follows then fail fast instead of fabricating local
    /// rows a remote server never hears about).
    pub ap: Option<&'a ApContext>,
}

impl<'a> Ctx<'a> {
    /// Build a context from HTTP state.
    pub fn from_state(state: &'a crate::http::AppState) -> Self {
        Ctx {
            db: &state.db,
            config: &state.config,
            ap: state.ap.as_ref(),
        }
    }

    /// Build a context from a job context and the handler's AP context.
    pub fn from_job(ctx: &'a crate::jobs::JobContext, ap: Option<&'a ApContext>) -> Self {
        Ctx {
            db: &ctx.db,
            config: &ctx.config,
            ap,
        }
    }

    fn require_ap(&self) -> Result<&'a ApContext> {
        self.ap
            .ok_or_else(|| Error::Internal("federation is disabled".to_string()))
    }
}

/// Outcome of [`follow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    /// A `follows` row exists now (new or already present).
    Following,
    /// A pending `follow_requests` row was (re)written — a locked local
    /// account, or a remote account that hasn't answered yet.
    Requested,
}

/// Optional flags on a follow (Mastodon's `show_reblogs`/`notify`).
#[derive(Debug, Clone, Copy, Default)]
pub struct FollowOpts {
    /// Receive the target's boosts in the home timeline.
    pub show_reblogs: Option<bool>,
    /// Notify on every new post.
    pub notify: Option<bool>,
}

/// Follow `target_id` on behalf of the local account `follower_id`.
/// Idempotent: existing follows and pending requests are no-ops that
/// still reconcile their flags.
pub async fn follow(
    ctx: Ctx<'_>,
    follower_id: i64,
    target_id: i64,
    opts: FollowOpts,
) -> Result<FollowOutcome> {
    if follower_id == target_id {
        return Err(Error::Validation("cannot follow yourself".to_string()));
    }
    let follower = account_row(ctx.db, follower_id)
        .await?
        .ok_or_else(|| Error::NotFound("follower account".into()))?;
    if follower.domain.is_some() {
        return Err(Error::Validation("follower must be a local account".into()));
    }
    let target = account_row(ctx.db, target_id)
        .await?
        .ok_or_else(|| Error::NotFound("follow target".into()))?;
    if target.suspended_at.is_some() || target.deleted_at.is_some() {
        return Err(Error::Validation(
            "the target account is suspended or deleted".to_string(),
        ));
    }
    // A block in either direction forbids the follow (Mastodon
    // semantics); the move-migration query also filters blockers in
    // SQL, but import/auto-follow go through here.
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM blocks \
         WHERE (account_id = $1 AND target_account_id = $2) \
            OR (account_id = $2 AND target_account_id = $1))",
    )
    .bind(follower_id)
    .bind(target_id)
    .fetch_one(ctx.db)
    .await?;
    if blocked {
        return Err(Error::Validation(
            "a block exists between these accounts".to_string(),
        ));
    }

    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
    )
    .bind(follower_id)
    .bind(target_id)
    .fetch_one(ctx.db)
    .await?;
    if already {
        // Flags still update — Mastodon's follow call doubles as the
        // "update relationship flags" path.
        sqlx::query(
            "UPDATE follows SET \
                 show_reblogs = COALESCE($3, show_reblogs), \
                 notify = COALESCE($4, notify), updated_at = now() \
             WHERE account_id = $1 AND target_account_id = $2",
        )
        .bind(follower_id)
        .bind(target_id)
        .bind(opts.show_reblogs)
        .bind(opts.notify)
        .execute(ctx.db)
        .await?;
        return Ok(FollowOutcome::Following);
    }

    let local_target = target.domain.is_none();
    if local_target && !target.locked {
        sqlx::query(
            "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify) \
             VALUES ($1, $2, COALESCE($3, true), COALESCE($4, false)) \
             ON CONFLICT (account_id, target_account_id) DO NOTHING",
        )
        .bind(follower_id)
        .bind(target_id)
        .bind(opts.show_reblogs)
        .bind(opts.notify)
        .execute(ctx.db)
        .await?;
        bump_counters(ctx.db, follower_id, target_id, 1).await?;
        return Ok(FollowOutcome::Following);
    }

    // Pending path: locked local target, or a remote target awaiting
    // the Accept handshake.
    let mut activity = Value::Null;
    if !local_target {
        ctx.require_ap()?;
        activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{}#follows/{}", follower.actor_id, uuid::Uuid::new_v4()),
            "type": "Follow",
            "actor": follower.actor_id,
            "object": target.actor_id,
            "to": [target.actor_id],
        });
    }
    // Insert first: the row is what `Accept`/`Reject` settles against.
    // A row without a wire Follow is retryable (the caller can unfollow
    // or re-follow); a published Follow with no row is a phantom the
    // remote may accept into nothing.
    sqlx::query(
        "INSERT INTO follow_requests (account_id, target_account_id, activity) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (account_id, target_account_id) DO UPDATE SET activity = EXCLUDED.activity",
    )
    .bind(follower_id)
    .bind(target_id)
    .bind(&activity)
    .execute(ctx.db)
    .await?;
    if !local_target {
        let ap = ctx.require_ap()?;
        if let Err(e) = delivery::publish(ap, &follower.username, &activity).await {
            // Roll the pending request back so the failed follow is
            // cleanly absent rather than pending forever.
            let _ = sqlx::query(
                "DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
            )
            .bind(follower_id)
            .bind(target_id)
            .execute(ctx.db)
            .await;
            return Err(e);
        }
    }
    Ok(FollowOutcome::Requested)
}

/// Unfollow `target_id`: drops both `follows` and `follow_requests`
/// rows and, for a remote target that had either, publishes an
/// `Undo(Follow)`. Returns whether anything was removed.
pub async fn unfollow(ctx: Ctx<'_>, follower_id: i64, target_id: i64) -> Result<bool> {
    let removed_follow =
        sqlx::query("DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2")
            .bind(follower_id)
            .bind(target_id)
            .execute(ctx.db)
            .await?
            .rows_affected()
            > 0;
    let removed_request =
        sqlx::query("DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2")
            .bind(follower_id)
            .bind(target_id)
            .execute(ctx.db)
            .await?
            .rows_affected()
            > 0;
    if !removed_follow && !removed_request {
        return Ok(false);
    }
    if removed_follow {
        bump_counters(ctx.db, follower_id, target_id, -1).await?;
    }

    let remote =
        sqlx::query_scalar::<_, bool>("SELECT domain IS NOT NULL FROM accounts WHERE id = $1")
            .bind(target_id)
            .fetch_one(ctx.db)
            .await?;
    if remote {
        if let Some(ap) = ctx.ap {
            let follower = account_row(ctx.db, follower_id).await?;
            let target = account_row(ctx.db, target_id).await?;
            if let (Some(follower), Some(target)) = (follower, target) {
                let activity = json!({
                    "@context": "https://www.w3.org/ns/activitystreams",
                    "id": format!("{}#unfollows/{}", follower.actor_id, uuid::Uuid::new_v4()),
                    "type": "Undo",
                    "actor": follower.actor_id,
                    "object": {
                        "type": "Follow",
                        "actor": follower.actor_id,
                        "object": target.actor_id,
                    },
                    "to": [target.actor_id],
                });
                if let Err(e) = delivery::publish(ap, &follower.username, &activity).await {
                    // The local state is already correct; a failed Undo
                    // is audit noise, not a reason to roll the unfollow
                    // back.
                    tracing::warn!(error = %e, "unfollow Undo publish failed");
                }
            }
        }
    }
    Ok(true)
}

/// The minimal `accounts` fields the relationship writers need.
struct AccountRow {
    username: String,
    actor_id: String,
    domain: Option<String>,
    locked: bool,
    suspended_at: Option<chrono::DateTime<chrono::Utc>>,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn account_row(pool: &PgPool, id: i64) -> Result<Option<AccountRow>> {
    let row = sqlx::query(
        "SELECT username, actor_id, domain, locked, suspended_at, deleted_at \
         FROM accounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AccountRow {
        username: r.get("username"),
        actor_id: r.get("actor_id"),
        domain: r.get("domain"),
        locked: r.get("locked"),
        suspended_at: r.get("suspended_at"),
        deleted_at: r.get("deleted_at"),
    }))
}

/// Best-effort counter maintenance (epic 14 owns the authoritative
/// recount); `delta` is +1 or −1.
async fn bump_counters(pool: &PgPool, follower_id: i64, target_id: i64, delta: i64) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET following_count = GREATEST(0, following_count + $2) \
         WHERE id = $1",
    )
    .bind(follower_id)
    .bind(delta)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE accounts SET followers_count = GREATEST(0, followers_count + $2) \
         WHERE id = $1",
    )
    .bind(target_id)
    .bind(delta)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve `input` — a `user@domain` handle (`@`-prefixed or not), a
/// bare local username, or an actor URL/URI — to an `accounts.id`,
/// fetching the remote document when needed. Per-row resolution
/// failures surface as errors the caller can count and continue past.
pub async fn resolve_account(ctx: Ctx<'_>, input: &str) -> Result<i64> {
    let input = input.trim().trim_start_matches('@');
    if input.is_empty() {
        return Err(Error::Validation("empty account address".to_string()));
    }

    // Actor URL / URI — local rows are matched on `actor_id` (the AP id)
    // or `url` (the profile URL remote software sometimes addresses).
    if input.contains("://") || input.starts_with("acct:") {
        let uri = input.strip_prefix("acct:").unwrap_or(input);
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM accounts WHERE (actor_id = $1 OR url = $1) AND deleted_at IS NULL",
        )
        .bind(uri)
        .fetch_optional(ctx.db)
        .await?;
        if let Some(id) = existing {
            return Ok(id);
        }
        return fetch_remote_actor(ctx, uri).await;
    }

    let local_domain = ctx.config.instance.domain.to_lowercase();
    match input.split_once('@') {
        Some((user, domain)) => {
            if domain.eq_ignore_ascii_case(&local_domain) {
                local_account_id(ctx.db, user).await
            } else {
                // A cached row resolves without a WebFinger round-trip.
                let cached: Option<i64> = sqlx::query_scalar(
                    "SELECT id FROM accounts \
                     WHERE lower(username) = lower($1) AND lower(domain) = lower($2) \
                     AND deleted_at IS NULL",
                )
                .bind(user)
                .bind(domain)
                .fetch_optional(ctx.db)
                .await?;
                if let Some(id) = cached {
                    return Ok(id);
                }
                let ap = ctx.require_ap()?;
                let client = delivery::http_client(ap);
                let user = user.to_string();
                let domain = domain.to_string();
                let actor_url = sync_call(move || {
                    pubby_sdk::webfinger::resolve_actor_url_with_client(&user, &domain, &client)
                });
                fetch_remote_actor(ctx, &actor_url).await
            }
        }
        None => local_account_id(ctx.db, input).await,
    }
}

async fn local_account_id(pool: &PgPool, username: &str) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE lower(username) = lower($1) AND domain IS NULL AND deleted_at IS NULL",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound(format!("account {username}")))
}

/// Fetch `actor_url` with the instance actor's signed client and
/// upsert the `accounts` row.
async fn fetch_remote_actor(ctx: Ctx<'_>, actor_url: &str) -> Result<i64> {
    let ap = ctx.require_ap()?;
    let handler = ap.multi.instance.clone();
    let url = actor_url.to_string();
    let doc = sync_call(move || handler.outbox.fetch_actor(&url))
        .ok_or_else(|| Error::NotFound(format!("remote actor {actor_url}")))?;
    activitypub::storage::upsert_remote_actor_row(ctx.db, actor_url, &doc)
        .await?
        .ok_or_else(|| Error::Internal(format!("upsert failed for {actor_url}")))
}

// ---------------------------------------------------------------------------
// Blocks, mutes, domain blocks — local rows only in this epic.
// ---------------------------------------------------------------------------

/// Block `target_id` for `account_id` (idempotent).
pub async fn block(pool: &PgPool, account_id: i64, target_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO blocks (account_id, target_account_id) VALUES ($1, $2) \
         ON CONFLICT (account_id, target_account_id) DO NOTHING",
    )
    .bind(account_id)
    .bind(target_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a block.
pub async fn unblock(pool: &PgPool, account_id: i64, target_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM blocks WHERE account_id = $1 AND target_account_id = $2")
        .bind(account_id)
        .bind(target_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mute `target_id` for `account_id` (idempotent).
pub async fn mute(
    pool: &PgPool,
    account_id: i64,
    target_id: i64,
    hide_notifications: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO mutes (account_id, target_account_id, hide_notifications) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (account_id, target_account_id) \
         DO UPDATE SET hide_notifications = EXCLUDED.hide_notifications",
    )
    .bind(account_id)
    .bind(target_id)
    .bind(hide_notifications)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a mute.
pub async fn unmute(pool: &PgPool, account_id: i64, target_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM mutes WHERE account_id = $1 AND target_account_id = $2")
        .bind(account_id)
        .bind(target_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Block a whole domain for `account_id` (idempotent).
pub async fn block_domain(pool: &PgPool, account_id: i64, domain: &str) -> Result<()> {
    let domain = domain.trim().to_lowercase();
    if domain.is_empty() {
        return Err(Error::Validation("empty domain".to_string()));
    }
    sqlx::query(
        "INSERT INTO account_domain_blocks (account_id, domain) VALUES ($1, $2) \
         ON CONFLICT (account_id, domain) DO NOTHING",
    )
    .bind(account_id)
    .bind(domain)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a user-level domain block.
pub async fn unblock_domain(pool: &PgPool, account_id: i64, domain: &str) -> Result<()> {
    sqlx::query("DELETE FROM account_domain_blocks WHERE account_id = $1 AND domain = $2")
        .bind(account_id)
        .bind(domain.to_lowercase())
        .execute(pool)
        .await?;
    Ok(())
}
