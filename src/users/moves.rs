//! Account moves (epic 11): `alsoKnownAs` aliases, the move-to flow
//! (password-confirmed, cooldown-limited, alias-verified), redirect
//! cancellation, and the `MigrateFollowers` job that migrates local
//! followers for both outbound and verified inbound moves.
//!
//! Mastodon's contract: a `Move` is only honored when the target's
//! `alsoKnownAs` names the origin — so inbound `Move`s are verified
//! against a *fresh* fetch of the target document before any follower
//! is touched (see `incoming::handle_move`, which only enqueues).

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{
    activitypub::{self, actors::sync_call, delivery, ApContext},
    auth::password::verify_password,
    http::AppState,
    jobs::{JobContext, JobHandler, JobKind},
    users::relationships::{self, Ctx, FollowOpts},
    users::transfer::str_list,
    Error, Result,
};

/// Mastodon's cooldown between moves.
const MOVE_COOLDOWN_DAYS: i64 = 30;

/// Followers migrated per `MigrateFollowers` run before re-enqueueing.
const MIGRATION_BATCH: i64 = 100;

/// `moved` in the Account entity is the target's nested Account —
/// resolved one level deep, never recursively.
pub async fn moved_account_json(
    pool: &PgPool,
    account: &crate::users::account::AccountRow,
    base_url: &str,
) -> Value {
    let Some(target_id) = account.moved_to_account_id else {
        return Value::Null;
    };
    match crate::users::account::load(pool, target_id).await {
        Ok(Some(target)) => crate::users::account::to_account(&target, base_url),
        _ => Value::Null,
    }
}

/// The caller's aliases, oldest first.
pub async fn list_aliases(pool: &PgPool, account_id: i64) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT uri FROM account_aliases WHERE account_id = $1 ORDER BY id")
            .bind(account_id)
            .fetch_all(pool)
            .await?,
    )
}

/// Add `acct` (handle or URI) to the caller's `alsoKnownAs`, then
/// publish an actor `Update` so the federation-facing document is
/// live. Idempotent.
pub async fn add_alias(state: &AppState, account_id: i64, acct: &str) -> Result<()> {
    let rel = relationships::Ctx::from_state(state);
    let target_id = relationships::resolve_account(rel, acct).await?;
    if target_id == account_id {
        return Err(Error::Validation("an account cannot alias itself".into()));
    }
    // Aliases name actor URIs, not local usernames.
    let uri: String = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(target_id)
        .fetch_one(&state.db)
        .await?;
    sqlx::query(
        "INSERT INTO account_aliases (account_id, uri) VALUES ($1, $2) \
         ON CONFLICT (account_id, uri) DO NOTHING",
    )
    .bind(account_id)
    .bind(&uri)
    .execute(&state.db)
    .await?;
    publish_update(state, account_id).await;
    Ok(())
}

/// Remove an alias by URI/handle/acct, then publish an actor `Update`.
pub async fn remove_alias(state: &AppState, account_id: i64, acct: &str) -> Result<()> {
    // Accept the alias URI itself or anything `resolve_account` maps
    // to an account.
    let uri =
        match sqlx::query_scalar::<_, String>("SELECT actor_id FROM accounts WHERE actor_id = $1")
            .bind(acct.trim())
            .fetch_optional(&state.db)
            .await?
        {
            Some(u) => u,
            None => {
                let rel = relationships::Ctx::from_state(state);
                match relationships::resolve_account(rel, acct).await {
                    Ok(id) => {
                        sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
                            .bind(id)
                            .fetch_one(&state.db)
                            .await?
                    }
                    Err(_) => acct.trim().to_string(),
                }
            }
        };
    sqlx::query("DELETE FROM account_aliases WHERE account_id = $1 AND uri = $2")
        .bind(account_id)
        .bind(&uri)
        .execute(&state.db)
        .await?;
    publish_update(state, account_id).await;
    Ok(())
}

/// `POST /api/v1/yonder/move`: point this account at `acct`.
///
/// 1. Password-confirmed; cooldown applies; self/deleted targets
///    refused.
/// 2. The target's *fresh* `alsoKnownAs` must name the caller.
/// 3. `moved_to_account_id`/`moved_at` set; actor `Update` + `Move`
///    (addressed to followers) published; `MigrateFollowers` enqueued.
pub async fn move_to(state: &AppState, account_id: i64, acct: &str, password: &str) -> Result<()> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
        ),
    >(
        "SELECT a.username, a.actor_id, a.moved_at, a.deleted_at, a.suspended_at, u.password_hash \
         FROM accounts a JOIN users u ON u.account_id = a.id \
         WHERE a.id = $1 AND a.domain IS NULL",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;
    let (username, actor_id, moved_at, deleted_at, suspended_at, password_hash) = row;
    if deleted_at.is_some() {
        return Err(Error::Validation("account is deleted".into()));
    }
    if suspended_at.is_some() {
        return Err(Error::Forbidden);
    }
    let Some(hash) = password_hash else {
        return Err(Error::Forbidden);
    };
    if !verify_password(password, &hash) {
        return Err(Error::Forbidden);
    }
    if let Some(at) = moved_at {
        let cooldown = chrono::Duration::days(MOVE_COOLDOWN_DAYS);
        if chrono::Utc::now() - at < cooldown {
            return Err(Error::RateLimited(format!(
                "account moved less than {MOVE_COOLDOWN_DAYS} days ago"
            )));
        }
    }

    // Resolve + verify the target's alsoKnownAs names this actor.
    let rel = relationships::Ctx::from_state(state);
    let target_id = relationships::resolve_account(rel, acct).await?;
    if target_id == account_id {
        return Err(Error::Validation("cannot move to yourself".into()));
    }
    let (target_actor_id, target_local): (String, bool) =
        sqlx::query_as("SELECT actor_id, domain IS NULL FROM accounts WHERE id = $1")
            .bind(target_id)
            .fetch_one(&state.db)
            .await?;
    let aliases: Vec<String> = if target_local {
        sqlx::query_scalar("SELECT uri FROM account_aliases WHERE account_id = $1")
            .bind(target_id)
            .fetch_all(&state.db)
            .await?
    } else {
        // Fresh fetch: drop any cached copy first so a stale document
        // can't grant or withhold a move the target never claimed.
        let ap = state
            .ap
            .as_ref()
            .ok_or_else(|| Error::Internal("federation is disabled".into()))?;
        sqlx::query("DELETE FROM actor_cache WHERE actor_id = $1")
            .bind(&target_actor_id)
            .execute(&state.db)
            .await?;
        let handler = ap.multi.instance.clone();
        let url = target_actor_id.clone();
        let doc = sync_call(move || handler.outbox.fetch_actor(&url)).ok_or_else(|| {
            Error::Validation(format!("could not fetch target actor {target_actor_id}"))
        })?;
        crate::activitypub::storage::upsert_remote_actor_row(&state.db, &target_actor_id, &doc)
            .await?;
        str_list(doc.get("alsoKnownAs"))
    };
    if !aliases.iter().any(|u| u == &actor_id) {
        return Err(Error::Validation(
            "the target account does not list this account in alsoKnownAs".into(),
        ));
    }

    sqlx::query(
        "UPDATE accounts SET moved_to_account_id = $2, moved_at = now(), updated_at = now() \
         WHERE id = $1",
    )
    .bind(account_id)
    .bind(target_id)
    .execute(&state.db)
    .await?;

    if let Some(ap) = state.ap.as_ref() {
        if let Err(e) = delivery::publish_actor_update(ap, &username).await {
            tracing::error!(error = %e, "actor Update publish failed");
        }
        let followers_url = format!("{actor_id}/followers");
        let seq: i64 = sqlx::query_scalar("SELECT count(*) FROM activities WHERE local")
            .fetch_one(&state.db)
            .await
            .unwrap_or(0)
            + 1;
        let activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor_id}#moves/{seq}"),
            "type": "Move",
            "actor": actor_id,
            "object": actor_id,
            "target": target_actor_id,
            "to": [followers_url],
            "cc": ["https://www.w3.org/ns/activitystreams#Public"],
        });
        if let Err(e) = delivery::publish(ap, &username, &activity).await {
            tracing::error!(error = %e, "Move publish failed");
        }
    }

    state
        .dispatcher
        .enqueue(
            JobKind::MigrateFollowers,
            json!({
                "direction": "local",
                "origin_id": account_id,
                "target_id": target_id,
            }),
        )
        .await?;
    Ok(())
}

/// Cancel the redirect: clears `moved_to_account_id` and republishes
/// the actor. Already-migrated followers stay where they went
/// (Mastodon semantics).
pub async fn cancel_move(state: &AppState, account_id: i64, password: &str) -> Result<()> {
    let password_hash: Option<String> = sqlx::query_scalar(
        "SELECT u.password_hash FROM accounts a JOIN users u ON u.account_id = a.id \
         WHERE a.id = $1 AND a.domain IS NULL",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;
    let Some(hash) = password_hash else {
        return Err(Error::Forbidden);
    };
    if !verify_password(password, &hash) {
        return Err(Error::Forbidden);
    }
    sqlx::query("UPDATE accounts SET moved_to_account_id = NULL, updated_at = now() WHERE id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;
    publish_update(state, account_id).await;
    Ok(())
}

async fn publish_update(state: &AppState, account_id: i64) {
    let Some(ap) = state.ap.as_ref() else { return };
    let username: Option<String> =
        sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);
    if let Some(username) = username {
        if let Err(e) = delivery::publish_actor_update(ap, &username).await {
            tracing::error!(error = %e, username, "actor Update publish failed");
        }
    }
}

// ---------------------------------------------------------------------------
// MigrateFollowers job
// ---------------------------------------------------------------------------

/// `JobKind::MigrateFollowers` — migrates local followers between two
/// accounts. `direction`:
///
/// - `"local"` (outbound move): `{origin_id, target_id}` — for every
///   local follower of origin, `follow(follower, target)` then
///   `unfollow(follower, origin)`.
/// - `"remote"` (inbound, already signature-checked): `{origin_actor,
///   target_actor, activity_id?}` — verifies the target's fresh
///   `alsoKnownAs ∋ origin` first, then migrates local followers.
///
/// Both directions batch through a `cursor` payload field (the
/// `follows.id` watermark) so large accounts migrate in bounded runs.
pub struct MigrateFollowersJob {
    ap: ApContext,
}

impl MigrateFollowersJob {
    /// Create the handler bound to the AP context.
    pub fn new(ap: ApContext) -> Self {
        MigrateFollowersJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for MigrateFollowersJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let direction = payload
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("");
        let cursor = payload.get("cursor").and_then(Value::as_i64).unwrap_or(0);
        match direction {
            "local" => {
                let origin_id = payload
                    .get("origin_id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Error::Validation("migrate job lacks origin_id".into()))?;
                let target_id = payload
                    .get("target_id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Error::Validation("migrate job lacks target_id".into()))?;
                migrate_local(ctx, &self.ap, origin_id, target_id, cursor, payload).await
            }
            "remote" => {
                let origin = payload
                    .get("origin_actor")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Validation("migrate job lacks origin_actor".into()))?;
                let target = payload
                    .get("target_actor")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Validation("migrate job lacks target_actor".into()))?;
                migrate_remote(ctx, &self.ap, origin, target, cursor, payload).await
            }
            other => Err(Error::Validation(format!(
                "unknown migrate direction {other:?}"
            ))),
        }
    }
}

/// Re-enqueue this job with an advanced cursor.
async fn continue_job(ctx: &JobContext, payload: &Value, cursor: i64) -> Result<()> {
    let mut next = payload.clone();
    next["cursor"] = json!(cursor);
    let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    dispatcher.enqueue(JobKind::MigrateFollowers, next).await?;
    Ok(())
}

/// Fetch `origin`'s and `target`'s accounts.id by actor URI.
async fn ids_for(pool: &PgPool, origin: &str, target: &str) -> Result<(i64, i64)> {
    let origin_id: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(origin)
        .fetch_optional(pool)
        .await?;
    let target_id: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(target)
        .fetch_optional(pool)
        .await?;
    match (origin_id, target_id) {
        (Some(o), Some(t)) => Ok((o, t)),
        _ => Err(Error::NotFound("origin or target account".into())),
    }
}

/// Migrate local followers `origin → target` in `follows.id` order.
async fn migrate_followers_batch(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    origin_id: i64,
    target_id: i64,
    cursor: i64,
    payload: &Value,
) -> Result<()> {
    let rel = Ctx {
        db: &ctx.db,
        config: &ctx.config,
        ap,
    };
    // Local followers of origin past the watermark. Followers who
    // block or mute the target don't migrate (Mastodon honors the
    // block/mute over the move).
    let rows = sqlx::query_as::<_, (i64, i64)>(
        "SELECT f.id, f.account_id FROM follows f \
         JOIN accounts a ON a.id = f.account_id \
         WHERE f.target_account_id = $1 AND a.domain IS NULL AND f.id > $3 \
         AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
         AND NOT EXISTS (SELECT 1 FROM blocks b \
             WHERE b.account_id = f.account_id AND b.target_account_id = $2) \
         AND NOT EXISTS (SELECT 1 FROM mutes m \
             WHERE m.account_id = f.account_id AND m.target_account_id = $2) \
         ORDER BY f.id LIMIT $4",
    )
    .bind(origin_id)
    .bind(target_id)
    .bind(cursor)
    .bind(MIGRATION_BATCH)
    .fetch_all(&ctx.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut watermark = cursor;
    for (follow_row, follower_id) in &rows {
        watermark = watermark.max(*follow_row);
        match relationships::follow(rel, *follower_id, target_id, FollowOpts::default()).await {
            Ok(_) => {
                if let Err(e) = relationships::unfollow(rel, *follower_id, origin_id).await {
                    tracing::warn!(follower_id, error = %e, "move: unfollow origin failed");
                }
            }
            Err(e) => {
                tracing::warn!(follower_id, error = %e, "move: follow target failed");
            }
        }
    }
    // A short batch means the migration is done — no trailing run.
    if rows.len() as i64 == MIGRATION_BATCH {
        continue_job(ctx, payload, watermark).await?;
    }
    Ok(())
}

/// `"local"` direction — follower migration for our own outbound move.
async fn migrate_local(
    ctx: &JobContext,
    ap: &ApContext,
    origin_id: i64,
    target_id: i64,
    cursor: i64,
    payload: &Value,
) -> Result<()> {
    migrate_followers_batch(ctx, Some(ap), origin_id, target_id, cursor, payload).await
}

/// `"remote"` direction — verify then migrate for an inbound `Move`.
async fn migrate_remote(
    ctx: &JobContext,
    ap: &ApContext,
    origin: &str,
    target: &str,
    cursor: i64,
    payload: &Value,
) -> Result<()> {
    // Verification runs once, on the first segment (cursor 0): fetch
    // the target document *fresh* and require `alsoKnownAs ∋ origin`.
    if cursor == 0 {
        // A *local* target's alsoKnownAs is its `account_aliases` rows
        // (which its actor document renders); a remote target is
        // fetched fresh — a cached copy could withhold a move the
        // target actually claimed, or grant one it revoked.
        let target_local: Option<i64> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1 AND domain IS NULL")
                .bind(target)
                .fetch_optional(&ctx.db)
                .await?;
        let verified = if let Some(target_id) = target_local {
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM account_aliases WHERE account_id = $1 AND uri = $2)",
            )
            .bind(target_id)
            .bind(origin)
            .fetch_one(&ctx.db)
            .await?
        } else {
            sqlx::query("DELETE FROM actor_cache WHERE actor_id = $1")
                .bind(target)
                .execute(&ctx.db)
                .await?;
            let handler = ap.multi.instance.clone();
            let url = target.to_string();
            let doc = sync_call(move || handler.outbox.fetch_actor(&url));
            match doc {
                Some(doc) => {
                    if !str_list(doc.get("alsoKnownAs")).iter().any(|u| u == origin) {
                        false
                    } else {
                        activitypub::storage::upsert_remote_actor_row(&ctx.db, target, &doc)
                            .await?;
                        true
                    }
                }
                None => false,
            }
        };
        if !verified {
            tracing::warn!(
                origin,
                target,
                "move target's alsoKnownAs does not name origin; dropping Move"
            );
            return Ok(());
        }
        // Verified — refresh the origin row best-effort (its document
        // may be gone already — a delete-and-move is legitimate) and
        // mark it moved.
        let origin_doc = {
            let handler = ap.multi.instance.clone();
            let url = origin.to_string();
            sync_call(move || handler.outbox.fetch_actor(&url))
        };
        if let Some(doc) = origin_doc {
            let _ = activitypub::storage::upsert_remote_actor_row(&ctx.db, origin, &doc).await;
        }
        // The origin row may still not exist (a Move from an actor we
        // never saw whose document is already gone) — nothing to mark
        // or migrate from, so drop the job rather than erroring it.
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1)")
                .bind(origin)
                .fetch_one(&ctx.db)
                .await?;
        if !exists {
            tracing::warn!(
                origin,
                target,
                "verified Move but origin row is unresolvable; dropping"
            );
            return Ok(());
        }
        sqlx::query(
            "UPDATE accounts SET moved_to_account_id = \
             (SELECT id FROM accounts WHERE actor_id = $2), moved_at = now(), updated_at = now() \
             WHERE actor_id = $1 AND domain IS NOT NULL",
        )
        .bind(origin)
        .bind(target)
        .execute(&ctx.db)
        .await?;
    }
    let (origin_id, target_id) = ids_for(&ctx.db, origin, target).await?;
    migrate_followers_batch(ctx, Some(ap), origin_id, target_id, cursor, payload).await
}

/// Register this module's job handler — needs the AP context (inbound
/// verification fetches documents; outbound unfollow publishes Undo).
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: &ApContext) {
    registry.register(
        JobKind::MigrateFollowers,
        MigrateFollowersJob::new(ap.clone()),
    );
}
