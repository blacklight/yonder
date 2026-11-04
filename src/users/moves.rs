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
    config: &crate::config::Config,
) -> Value {
    let Some(target_id) = account.moved_to_account_id else {
        return Value::Null;
    };
    match crate::users::account::load(pool, target_id).await {
        Ok(Some(target)) => {
            // One level only — emoji pass but no recursive `moved`.
            let mut entity = crate::users::account::to_account(&target, base_url, config);
            let texts = crate::users::account::emoji_texts(&target);
            match crate::emoji::render::EmojiMap::for_author_texts(
                pool,
                config,
                base_url,
                target.domain.as_deref(),
                &texts,
            )
            .await
            {
                Ok(map) => crate::emoji::render::apply_account_with_map(&mut entity, &map),
                Err(e) => {
                    tracing::warn!(account_id = target.id, error = %e, "moved-account emoji map failed")
                }
            }
            entity
        }
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
    let cfg = state.config();
    let rel = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
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
                let cfg = state.config();
                let rel = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
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
        return Err(Error::Forbidden("account is suspended"));
    }
    let Some(hash) = password_hash else {
        return Err(Error::Forbidden("incorrect password"));
    };
    if !verify_password(password, &hash) {
        return Err(Error::Forbidden("incorrect password"));
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
    let cfg = state.config();
    let rel = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
    let target_id = relationships::resolve_account(rel, acct).await?;
    if target_id == account_id {
        return Err(Error::Validation("cannot move to yourself".into()));
    }
    let (target_actor_id, target_local, target_available, target_moved): (
        String,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT actor_id, domain IS NULL, \
         suspended_at IS NULL AND deleted_at IS NULL, \
         moved_to_account_id IS NULL FROM accounts WHERE id = $1",
    )
    .bind(target_id)
    .fetch_one(&state.db)
    .await?;
    if !target_available {
        return Err(Error::Validation(
            "the target account is suspended or deleted".into(),
        ));
    }
    if !target_moved {
        return Err(Error::Validation(
            "the target account has itself moved".into(),
        ));
    }
    // Federation-off moves stay local-only: a remote target would need
    // the AP path both for the signed Move and for Follow/Accept.
    let federating = state.ap.is_some();
    if !federating && !target_local {
        return Err(Error::Validation("federation is disabled".into()));
    }
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
        crate::activitypub::storage::upsert_remote_actor_row(
            &state.db,
            &state.config(),
            &target_actor_id,
            &doc,
        )
        .await?;
        str_list(doc.get("alsoKnownAs"))
    };
    if !aliases.iter().any(|u| u == &actor_id) {
        return Err(Error::Validation(
            "the target account does not list this account in alsoKnownAs".into(),
        ));
    }

    // One durable step: the redirect, the migration event, and the
    // publication-work record commit together so a crash cannot leave
    // a cooldown-only state with nothing to retry.
    let mut tx = state.db.begin().await?;
    // Serialize concurrent moves for this account: the cooldown and
    // live-event checks above ran without a lock, so re-verify under
    // the origin row lock before committing anything.
    let (locked_moved_at, live_event): (Option<chrono::DateTime<chrono::Utc>>, bool) =
        sqlx::query_as(
            "SELECT a.moved_at, EXISTS(SELECT 1 FROM migration_events e \
         WHERE e.source_account_id = a.id \
         AND e.state IN ('pending', 'publishing', 'migrating')) \
         FROM accounts a WHERE a.id = $1 FOR UPDATE",
        )
        .bind(account_id)
        .fetch_one(&mut *tx)
        .await?;
    if let Some(at) = locked_moved_at {
        if chrono::Utc::now() - at < chrono::Duration::days(MOVE_COOLDOWN_DAYS) {
            return Err(Error::RateLimited(format!(
                "account moved less than {MOVE_COOLDOWN_DAYS} days ago"
            )));
        }
    }
    if live_event {
        return Err(Error::Conflict("a move is already in progress".into()));
    }
    sqlx::query(
        "UPDATE accounts SET moved_to_account_id = $2, moved_at = now(), updated_at = now() \
         WHERE id = $1",
    )
    .bind(account_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;
    let event_id: i64 = sqlx::query_scalar(
        "INSERT INTO migration_events (source_account_id, target_account_id, state) \
         VALUES ($1, $2, 'publishing') RETURNING id",
    )
    .bind(account_id)
    .bind(target_id)
    .fetch_one(&mut *tx)
    .await?;
    let work_id: Option<i64> = if federating {
        let followers_url = format!("{actor_id}/followers");
        // The Move's identity derives from the stable event id, not a
        // global activity count.
        let activity = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor_id}#moves/{event_id}"),
            "type": "Move",
            "actor": actor_id,
            "object": actor_id,
            "target": target_actor_id,
            "to": [followers_url],
            "cc": ["https://www.w3.org/ns/activitystreams#Public"],
        });
        Some(
            sqlx::query_scalar(
                "INSERT INTO publication_work (activity_type, activity_id, payload) \
                 VALUES ('Move', $1, $2) RETURNING id",
            )
            .bind(activity["id"].as_str().unwrap())
            .bind(json!({"activity": activity, "username": username}))
            .fetch_one(&mut *tx)
            .await?,
        )
    } else {
        None
    };
    sqlx::query("UPDATE migration_events SET progress = $2 WHERE id = $1")
        .bind(event_id)
        .bind(json!({"work_id": work_id, "migration_enqueued": false}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    if let Some(ap) = state.ap.as_ref() {
        // Publish + enqueue now; whatever fails stays pending and is
        // retried by the PublishMove backstop enqueued below.
        if let Err(e) = advance_move(&state.db, ap, &state.dispatcher, event_id).await {
            tracing::error!(event_id, error = %e, "move advance failed; PublishMove retries");
        }
        if let Err(e) = state
            .dispatcher
            .enqueue(JobKind::PublishMove, json!({ "event_id": event_id }))
            .await
        {
            tracing::error!(event_id, error = %e, "PublishMove backstop enqueue failed");
        }
        return Ok(());
    }

    // Federation disabled: migrate local followers inline — both
    // accounts are local, so no AP work is needed. There is no worker
    // to retry with, so failures land as terminal outcomes and the
    // event finishes as 'failed' rather than claiming success.
    let ctx = JobContext {
        db: state.db.clone(),
        redis: state.redis.clone(),
        config: state.config().clone(),
        media: state.media.clone(),
    };
    let mut cursor = 0;
    while let BatchStep::Continue { cursor: next, .. } =
        run_follower_batch(&ctx, None, event_id, account_id, target_id, cursor, false).await?
    {
        cursor = next;
    }
    Ok(())
}

/// `follow(follower, target)` + observer-state carry-over +
/// `unfollow(follower, origin)` for one follower. `target_local`
/// decides between a direct `follows` insert — a verified move
/// migrates into locked local targets too, never a fresh request —
/// and a normal `relationships::follow` for remote targets (real
/// Follow/Accept).
async fn migrate_one(
    ctx: Ctx<'_>,
    follower_id: i64,
    origin_id: i64,
    target_id: i64,
    target_local: bool,
    opts: FollowOpts,
) -> Result<()> {
    if target_local {
        // An existing follow keeps its own preferences — the origin's
        // flags only seed a brand-new follows row.
        sqlx::query(
            "INSERT INTO follows (account_id, target_account_id, show_reblogs, notify) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (account_id, target_account_id) DO NOTHING",
        )
        .bind(follower_id)
        .bind(target_id)
        .bind(opts.show_reblogs.unwrap_or(true))
        .bind(opts.notify.unwrap_or(false))
        .execute(ctx.db)
        .await?;
        // A stale pending request to the same target is settled by
        // the migration itself.
        sqlx::query("DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2")
            .bind(follower_id)
            .bind(target_id)
            .execute(ctx.db)
            .await?;
    } else {
        relationships::follow(ctx, follower_id, target_id, opts).await?;
    }
    carry_observer_state(ctx.db, follower_id, origin_id, target_id).await?;
    relationships::unfollow(ctx, follower_id, origin_id).await?;
    Ok(())
}

/// Mastodon MoveWorker parity for local observers: the follower's
/// note on the moved person, blocks/mutes of the moved account, and
/// list memberships all point at the target now — the account moved,
/// the relationships didn't lapse. All idempotent.
async fn carry_observer_state(
    db: &PgPool,
    follower_id: i64,
    origin_id: i64,
    target_id: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO account_notes (account_id, target_account_id, comment) \
         SELECT account_id, $3, comment FROM account_notes \
         WHERE account_id = $1 AND target_account_id = $2 \
         ON CONFLICT (account_id, target_account_id) DO NOTHING",
    )
    .bind(follower_id)
    .bind(origin_id)
    .bind(target_id)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO blocks (account_id, target_account_id) \
         SELECT account_id, $3 FROM blocks \
         WHERE account_id = $1 AND target_account_id = $2 \
         ON CONFLICT DO NOTHING",
    )
    .bind(follower_id)
    .bind(origin_id)
    .bind(target_id)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO mutes (account_id, target_account_id) \
         SELECT account_id, $3 FROM mutes \
         WHERE account_id = $1 AND target_account_id = $2 \
         ON CONFLICT DO NOTHING",
    )
    .bind(follower_id)
    .bind(origin_id)
    .bind(target_id)
    .execute(db)
    .await?;
    sqlx::query(
        "INSERT INTO list_accounts (list_id, account_id) \
         SELECT la.list_id, $3 FROM list_accounts la \
         JOIN lists l ON l.id = la.list_id \
         WHERE l.account_id = $1 AND la.account_id = $2 \
         ON CONFLICT DO NOTHING",
    )
    .bind(follower_id)
    .bind(origin_id)
    .bind(target_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Followers who block/mute the target (or are blocked by it) never
/// migrate — record them as `skipped` outcomes so the move report
/// distinguishes "didn't move" from "not yet tried". The destination
/// itself can follow the source (a person following their own
/// account); migrating it would violate the self-follow constraint,
/// so it is skipped the same way.
async fn record_skipped(db: &PgPool, event_id: i64, origin_id: i64, target_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO migration_follower_outcomes (event_id, follower_account_id, state) \
         SELECT $1, f.account_id, 'skipped' FROM follows f \
         JOIN accounts a ON a.id = f.account_id \
         WHERE f.target_account_id = $2 AND a.domain IS NULL \
         AND (f.account_id = $3 \
          OR EXISTS (SELECT 1 FROM blocks b \
             WHERE (b.account_id = f.account_id AND b.target_account_id = $3) \
                OR (b.account_id = $3 AND b.target_account_id = f.account_id)) \
          OR EXISTS (SELECT 1 FROM mutes m \
             WHERE m.account_id = f.account_id AND m.target_account_id = $3)) \
         ON CONFLICT (event_id, follower_account_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(origin_id)
    .bind(target_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Retries allowed after the first failed attempt before the outcome
/// is left permanently `failed` (reported via move_state). `attempts`
/// counts *past* the initial try (the INSERT leaves it at 0), so a
/// follower is tried at most `MAX_FOLLOWER_RETRIES + 1` times.
const MAX_FOLLOWER_RETRIES: i32 = 5;
/// Retry backoff for failed follower migration (seconds).
const RETRY_BASE_SECS: u64 = 30;
const RETRY_MAX_SECS: u64 = 900;

/// Record one follower's outcome (attempts count each try). A failed
/// outcome gets `next_retry_at` on a capped exponential backoff when
/// `retry_supported`; otherwise — or past the cap — `next_retry_at`
/// stays NULL, marking a permanent failure that the batch no longer
/// retries but `move_state` still reports.
async fn outcome(
    db: &PgPool,
    event_id: i64,
    follower_id: i64,
    state: &str,
    error: Option<&crate::Error>,
    retry_supported: bool,
) -> Result<()> {
    let attempts: i32 = sqlx::query_scalar(
        "INSERT INTO migration_follower_outcomes \
         (event_id, follower_account_id, state, attempts, next_retry_at, error) \
         VALUES ($1, $2, $3, 0, NULL, $4) \
         ON CONFLICT (event_id, follower_account_id) \
         DO UPDATE SET state = EXCLUDED.state, error = EXCLUDED.error, \
             attempts = migration_follower_outcomes.attempts + 1, \
             next_retry_at = NULL, updated_at = now() \
         RETURNING attempts",
    )
    .bind(event_id)
    .bind(follower_id)
    .bind(state)
    .bind(error.map(|e| e.to_string()))
    .fetch_one(db)
    .await?;
    if state == "failed" && retry_supported && attempts < MAX_FOLLOWER_RETRIES {
        let secs =
            crate::jobs::queue::backoff_seconds(attempts as u32, RETRY_BASE_SECS, RETRY_MAX_SECS);
        sqlx::query(
            "UPDATE migration_follower_outcomes \
             SET next_retry_at = now() + make_interval(secs => $3) \
             WHERE event_id = $1 AND follower_account_id = $2",
        )
        .bind(event_id)
        .bind(follower_id)
        .bind(secs as f64)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Publish the pending `publication_work` row for `event_id`, then
/// advance the event into the migrating phase (enqueueing
/// `MigrateFollowers` exactly once). Called inline from `move_to`
/// and retried by [`PublishMoveJob`].
async fn advance_move(
    db: &PgPool,
    ap: &ApContext,
    dispatcher: &crate::jobs::Dispatcher,
    event_id: i64,
) -> Result<()> {
    let event = sqlx::query_as::<_, (String, i64, i64, Value)>(
        "SELECT state, source_account_id, target_account_id, progress \
         FROM migration_events WHERE id = $1",
    )
    .bind(event_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("migration event".into()))?;
    let (state, origin_id, target_id, progress) = event;
    if matches!(state.as_str(), "done" | "cancelled" | "failed") {
        return Ok(());
    }

    let work_id = progress.get("work_id").and_then(Value::as_i64);
    if let Some(work_id) = work_id {
        let work = sqlx::query_as::<_, (String, String, Value)>(
            "SELECT state, activity_id, payload FROM publication_work WHERE id = $1",
        )
        .bind(work_id)
        .fetch_optional(db)
        .await?;
        if let Some((work_state, activity_id, payload)) = work {
            if work_state == "pending" {
                // Cancel may have landed between the event read and
                // now — re-check before publishing anything.
                let live: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM migration_events WHERE id = $1 \
                     AND state IN ('pending', 'publishing', 'migrating'))",
                )
                .bind(event_id)
                .fetch_one(db)
                .await?;
                if !live {
                    return Ok(());
                }
                let username = payload["username"].as_str().unwrap_or("");
                // A half-finished earlier attempt may have stored the
                // activity already — `activities.activity_id` is
                // unique, so re-publishing blindly would error.
                let stored: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM activities WHERE activity_id = $1)",
                )
                .bind(&activity_id)
                .fetch_one(db)
                .await?;
                if !stored {
                    delivery::publish(ap, username, &payload["activity"]).await?;
                }
                // The actor Update is part of the same work item: a
                // failure leaves the row pending so the PublishMove
                // backstop retries it instead of losing the Update.
                delivery::publish_actor_update(ap, username).await?;
                sqlx::query(
                    "UPDATE publication_work SET state = 'delivered', updated_at = now() \
                     WHERE id = $1",
                )
                .bind(work_id)
                .execute(db)
                .await?;
            }
        }
    }

    if state == "publishing" {
        if !progress
            .get("migration_enqueued")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            dispatcher
                .enqueue(
                    JobKind::MigrateFollowers,
                    json!({
                        "direction": "local",
                        "origin_id": origin_id,
                        "target_id": target_id,
                        "event_id": event_id,
                    }),
                )
                .await?;
        }
        sqlx::query(
            "UPDATE migration_events SET state = 'migrating', \
             progress = progress || '{\"migration_enqueued\": true}'::jsonb, \
             updated_at = now() WHERE id = $1",
        )
        .bind(event_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// `JobKind::PublishMove` — the durable backstop: republishes pending
/// move work and ensures `MigrateFollowers` was enqueued. Enqueue
/// failures and publish errors surface as job errors (retry).
pub struct PublishMoveJob {
    ap: ApContext,
}

impl PublishMoveJob {
    /// Create the handler bound to the AP context.
    pub fn new(ap: ApContext) -> Self {
        PublishMoveJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for PublishMoveJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let event_id = payload
            .get("event_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("publish_move job lacks event_id".into()))?;
        let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        advance_move(&ctx.db, &self.ap, &dispatcher, event_id).await
    }
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
        return Err(Error::Forbidden("incorrect password"));
    };
    if !verify_password(password, &hash) {
        return Err(Error::Forbidden("incorrect password"));
    }
    sqlx::query("UPDATE accounts SET moved_to_account_id = NULL, updated_at = now() WHERE id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;
    // Cancel unstarted work: live events stop migrating (the batch
    // checks event state each run) and pending publications never
    // fire. Already-migrated followers and delivered Moves stay —
    // Mastodon semantics.
    let events = sqlx::query_as::<_, (i64, Value)>(
        "SELECT id, progress FROM migration_events \
         WHERE source_account_id = $1 AND state IN ('pending', 'publishing', 'migrating')",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    for (event_id, progress) in events {
        if let Some(work_id) = progress.get("work_id").and_then(Value::as_i64) {
            sqlx::query(
                "UPDATE publication_work SET state = 'failed', error = 'cancelled', \
                 updated_at = now() WHERE id = $1 AND state = 'pending'",
            )
            .bind(work_id)
            .execute(&state.db)
            .await?;
        }
        sqlx::query(
            "UPDATE migration_events SET state = 'cancelled', updated_at = now() WHERE id = $1",
        )
        .bind(event_id)
        .execute(&state.db)
        .await?;
    }
    publish_update(state, account_id).await;
    Ok(())
}

/// `GET /api/v1/yonder/move` — the account's redirect, cooldown and
/// latest migration event so settings can render progress.
pub async fn move_state(state: &AppState, account_id: i64) -> Result<Value> {
    let (moved_at, moved_to, username): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "SELECT a.moved_at, \
         (SELECT actor_id FROM accounts WHERE id = a.moved_to_account_id), \
         a.username \
         FROM accounts a WHERE a.id = $1 AND a.domain IS NULL",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let cooldown_until = moved_at
        .map(|at| at + chrono::Duration::days(MOVE_COOLDOWN_DAYS))
        .filter(|until| *until > chrono::Utc::now());
    let event = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
        ),
    >(
        "SELECT id, state, created_at, error FROM migration_events \
         WHERE source_account_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let (event_json, followers_json) = match &event {
        Some((id, ev_state, _, error)) => {
            let counts = sqlx::query_as::<_, (i64, i64, i64, i64)>(
                "SELECT \
                 count(*) FILTER (WHERE state = 'migrated'), \
                 count(*) FILTER (WHERE state = 'skipped'), \
                 count(*) FILTER (WHERE state = 'failed'), \
                 count(*) FILTER (WHERE state = 'pending') \
                 FROM migration_follower_outcomes WHERE event_id = $1",
            )
            .bind(id)
            .fetch_one(&state.db)
            .await?;
            (
                json!({
                    "id": id.to_string(),
                    "state": ev_state,
                    "error": error,
                }),
                json!({
                    "migrated": counts.0,
                    "skipped": counts.1,
                    "failed": counts.2,
                    "pending": counts.3,
                }),
            )
        }
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "acct": username,
        "moved": moved_to.is_some(),
        "target": moved_to,
        "moved_at": moved_at.map(|t| t.to_rfc3339()),
        "cooldown_until": cooldown_until.map(|t| t.to_rfc3339()),
        "event": event_json,
        "followers": followers_json,
    }))
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
                // Pre-event payloads (or hand-driven runs) carry no
                // event_id — fall back to the pair's live event.
                let event_id = match payload.get("event_id").and_then(Value::as_i64) {
                    Some(id) => id,
                    None => live_event(&ctx.db, origin_id, target_id).await?,
                };
                migrate_local(
                    ctx, &self.ap, event_id, origin_id, target_id, cursor, payload,
                )
                .await
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

/// The pair's latest migration event — the fallback for legacy
/// payloads that predate `event_id`. Terminal events return too: the
/// batch no-ops on them, so replays coalesce rather than error.
async fn live_event(db: &PgPool, origin_id: i64, target_id: i64) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT id FROM migration_events \
         WHERE source_account_id = $1 AND target_account_id = $2 \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(origin_id)
    .bind(target_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("live migration event".into()))
}

/// What one follower-migration batch decided.
enum BatchStep {
    /// Re-run from `cursor` — immediately, or once `delay` has passed
    /// (the next deferred-retry deadline).
    Continue {
        cursor: i64,
        delay: Option<std::time::Duration>,
    },
    /// The event reached a terminal state (or there is nothing left).
    Finished,
}

/// One pass over the local followers of `origin` in `follows.id`
/// order past `cursor`, recording each result in
/// `migration_follower_outcomes`. The watermark only advances past
/// *resolved* rows: a failed or not-yet-due retry blocks it, so the
/// next pass retries the follow instead of losing it. Failures are
/// retried on a capped backoff (`outcome`) when `retry_supported`;
/// without a worker (`retry_supported = false`, the federation-off
/// inline path) they go terminal immediately.
async fn run_follower_batch(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    event_id: i64,
    origin_id: i64,
    target_id: i64,
    cursor: i64,
    retry_supported: bool,
) -> Result<BatchStep> {
    // Terminal events stop all further migration work — replays and
    // retries coalesce onto them instead of re-migrating.
    let event_state: Option<String> =
        sqlx::query_scalar("SELECT state FROM migration_events WHERE id = $1")
            .bind(event_id)
            .fetch_optional(&ctx.db)
            .await?;
    if matches!(
        event_state.as_deref(),
        Some("cancelled" | "done" | "failed") | None
    ) {
        return Ok(BatchStep::Finished);
    }

    // A target that can no longer accept follows fails the whole
    // event — per-follower retries cannot fix suspension or deletion.
    let (target_local, target_available): (bool, bool) = sqlx::query_as(
        "SELECT domain IS NULL, suspended_at IS NULL AND deleted_at IS NULL \
         FROM accounts WHERE id = $1",
    )
    .bind(target_id)
    .fetch_one(&ctx.db)
    .await?;
    if !target_available {
        fail_event(
            &ctx.db,
            event_id,
            "the target account is suspended or deleted",
        )
        .await?;
        return Ok(BatchStep::Finished);
    }

    let rel = Ctx::for_parts(&ctx.db, &ctx.config, ap);
    record_skipped(&ctx.db, event_id, origin_id, target_id).await?;

    // Local followers of origin past the watermark. Followers who
    // block or mute the target don't migrate (Mastodon honors the
    // block/mute over the move); their `show_reblogs`/`notify` flags
    // carry to the new follow. Permanently failed outcomes
    // (`next_retry_at IS NULL`) are skipped so they can't wedge the
    // cursor; `failed_retry_at` marks rows deferred to a later pass.
    let rows = sqlx::query_as::<_, (i64, i64, bool, bool, Option<chrono::DateTime<chrono::Utc>>)>(
        "SELECT f.id, f.account_id, f.show_reblogs, f.notify, \
         (SELECT o.next_retry_at FROM migration_follower_outcomes o \
          WHERE o.event_id = $4 AND o.follower_account_id = f.account_id \
          AND o.state = 'failed') AS failed_retry_at \
         FROM follows f \
         JOIN accounts a ON a.id = f.account_id \
         WHERE f.target_account_id = $1 AND a.domain IS NULL AND f.id > $3 \
         AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
         AND f.account_id <> $2 \
         AND NOT EXISTS (SELECT 1 FROM blocks b \
             WHERE b.account_id = f.account_id AND b.target_account_id = $2) \
         AND NOT EXISTS (SELECT 1 FROM mutes m \
             WHERE m.account_id = f.account_id AND m.target_account_id = $2) \
         AND NOT EXISTS (SELECT 1 FROM migration_follower_outcomes o \
             WHERE o.event_id = $4 AND o.follower_account_id = f.account_id \
             AND o.state = 'failed' AND o.next_retry_at IS NULL) \
         ORDER BY f.id LIMIT $5",
    )
    .bind(origin_id)
    .bind(target_id)
    .bind(cursor)
    .bind(event_id)
    .bind(MIGRATION_BATCH)
    .fetch_all(&ctx.db)
    .await?;
    // `resolved` only advances over contiguously finished rows: a
    // failure or a not-yet-due retry keeps the watermark behind it.
    let mut resolved = cursor;
    let mut blocked = false;
    let now = chrono::Utc::now();
    let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    for (follow_row, follower_id, show_reblogs, notify, failed_retry_at) in &rows {
        // A failed outcome not yet due for retry stays put — it still
        // blocks the watermark so the deferred pass revisits it.
        if let Some(at) = failed_retry_at {
            if *at > now {
                blocked = true;
                continue;
            }
        }
        let opts = FollowOpts {
            show_reblogs: Some(*show_reblogs),
            notify: Some(*notify),
        };
        match migrate_one(rel, *follower_id, origin_id, target_id, target_local, opts).await {
            Ok(()) => {
                outcome(
                    &ctx.db,
                    event_id,
                    *follower_id,
                    "migrated",
                    None,
                    retry_supported,
                )
                .await?;
                // Akkoma-style `move` notification — deduped per
                // (follower, origin, event) so retries never double.
                if let Err(e) = dispatcher
                    .enqueue(
                        JobKind::NotificationFanout,
                        json!({
                            "type": "move",
                            "from_account_id": origin_id,
                            "account_id": follower_id,
                            "migration_event_id": event_id,
                            "target_account_id": target_id,
                        }),
                    )
                    .await
                {
                    tracing::warn!(follower_id, error = %e, "move notification enqueue failed");
                }
                if !blocked {
                    resolved = *follow_row;
                }
            }
            Err(e) => {
                tracing::warn!(follower_id, error = %e, "move: migration failed; retained for retry");
                outcome(
                    &ctx.db,
                    event_id,
                    *follower_id,
                    "failed",
                    Some(&e),
                    retry_supported,
                )
                .await?;
                blocked = true;
            }
        }
    }
    sqlx::query("UPDATE migration_events SET cursor = $2, updated_at = now() WHERE id = $1")
        .bind(event_id)
        .bind(resolved)
        .execute(&ctx.db)
        .await?;
    if rows.len() as i64 == MIGRATION_BATCH {
        return Ok(BatchStep::Continue {
            cursor: resolved,
            delay: None,
        });
    }
    // Anything left with a retry deadline gets one delayed pass; the
    // rest is terminal, so the event can finish.
    let next_due: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT min(next_retry_at) FROM migration_follower_outcomes \
         WHERE event_id = $1 AND state = 'failed' AND next_retry_at IS NOT NULL",
    )
    .bind(event_id)
    .fetch_one(&ctx.db)
    .await?;
    if let Some(at) = next_due {
        let delay = (at - now).to_std().unwrap_or_default();
        return Ok(BatchStep::Continue {
            cursor: resolved,
            delay: Some(delay),
        });
    }
    finish_event(&ctx.db, event_id).await?;
    Ok(BatchStep::Finished)
}

/// Job-handler wrapper around [`run_follower_batch`]: translate the
/// step into a (possibly delayed) continuation enqueue.
async fn migrate_followers_batch(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    event_id: i64,
    origin_id: i64,
    target_id: i64,
    cursor: i64,
    payload: &Value,
) -> Result<()> {
    match run_follower_batch(ctx, ap, event_id, origin_id, target_id, cursor, true).await? {
        BatchStep::Finished => Ok(()),
        BatchStep::Continue {
            cursor: next,
            delay: None,
        } => continue_job(ctx, payload, next).await,
        BatchStep::Continue {
            cursor: next,
            delay: Some(delay),
        } => continue_job_delayed(ctx, payload, next, delay).await,
    }
}

/// Re-enqueue this job once `delay` has passed.
async fn continue_job_delayed(
    ctx: &JobContext,
    payload: &Value,
    cursor: i64,
    delay: std::time::Duration,
) -> Result<()> {
    let mut next = payload.clone();
    next["cursor"] = json!(cursor);
    let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    dispatcher
        .enqueue_delayed(JobKind::MigrateFollowers, next, delay)
        .await?;
    Ok(())
}

/// Mark the event permanently failed — a terminal state `move_state`
/// reports (outcome counts stay queryable).
async fn fail_event(db: &PgPool, event_id: i64, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE migration_events SET state = 'failed', error = $2, \
         completed_at = now(), updated_at = now() \
         WHERE id = $1 AND state IN ('pending', 'publishing', 'migrating')",
    )
    .bind(event_id)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// Finish the event once no retryable work remains: 'done' when every
/// outcome resolved, 'failed' when permanent failures exist — either
/// way `move_state` reports the outcome counts.
async fn finish_event(db: &PgPool, event_id: i64) -> Result<()> {
    let (pending, failed): (i64, i64) = sqlx::query_as(
        "SELECT \
         count(*) FILTER (WHERE state = 'pending' \
             OR (state = 'failed' AND next_retry_at IS NOT NULL)), \
         count(*) FILTER (WHERE state = 'failed' AND next_retry_at IS NULL) \
         FROM migration_follower_outcomes WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(db)
    .await?;
    if pending != 0 {
        return Ok(());
    }
    if failed == 0 {
        sqlx::query(
            "UPDATE migration_events SET state = 'done', completed_at = now(), \
             updated_at = now() WHERE id = $1 AND state IN ('publishing', 'migrating')",
        )
        .bind(event_id)
        .execute(db)
        .await?;
    } else {
        fail_event(db, event_id, "some followers could not be migrated").await?;
    }
    Ok(())
}

/// `"local"` direction — follower migration for our own outbound move.
async fn migrate_local(
    ctx: &JobContext,
    ap: &ApContext,
    event_id: i64,
    origin_id: i64,
    target_id: i64,
    cursor: i64,
    payload: &Value,
) -> Result<()> {
    migrate_followers_batch(
        ctx,
        Some(ap),
        event_id,
        origin_id,
        target_id,
        cursor,
        payload,
    )
    .await
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
    let activity_id = payload
        .get("activity_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // Replays need a stable dedupe key even when the remote omitted
    // the activity id: (origin, target) identifies the claimed move.
    let dedupe_key = activity_id
        .clone()
        .unwrap_or_else(|| format!("yonder-move:{origin}:{target}"));

    // Verification + event creation run once, on the first segment
    // (cursor 0): fetch the target document *fresh* and require
    // `alsoKnownAs ∋ origin`. A *local* target's alsoKnownAs is its
    // `account_aliases` rows; a remote target is fetched fresh — a
    // cached copy could withhold a move the target actually claimed,
    // or grant one it revoked.
    let event_id = if cursor == 0 {
        // Replay check first: a verified event for this move already
        // exists — coalesce onto it instead of re-verifying.
        let existing = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, state FROM migration_events WHERE source_activity_id = $1",
        )
        .bind(&dedupe_key)
        .fetch_optional(&ctx.db)
        .await?;
        if let Some((id, state)) = existing {
            if matches!(state.as_str(), "done" | "cancelled" | "failed") {
                return Ok(());
            }
            id
        } else {
            let target_local: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM accounts WHERE actor_id = $1 AND domain IS NULL",
            )
            .bind(target)
            .fetch_optional(&ctx.db)
            .await?;
            let verified = if let Some(target_id) = target_local {
                sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM account_aliases \
                     WHERE account_id = $1 AND uri = $2)",
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
                            activitypub::storage::upsert_remote_actor_row(
                                &ctx.db,
                                &ctx.config,
                                target,
                                &doc,
                            )
                            .await?;
                            true
                        }
                    }
                    // Fetch failed — transient until proven otherwise:
                    // erroring lets the job retry rather than
                    // acknowledging a permanent alias rejection.
                    None => {
                        return Err(Error::Internal(format!(
                            "could not fetch move target {target}"
                        )))
                    }
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
            // Verified — refresh the origin row best-effort (its
            // document may be gone already — a delete-and-move is
            // legitimate).
            let origin_doc = {
                let handler = ap.multi.instance.clone();
                let url = origin.to_string();
                sync_call(move || handler.outbox.fetch_actor(&url))
            };
            if let Some(doc) = origin_doc {
                let _ = activitypub::storage::upsert_remote_actor_row(
                    &ctx.db,
                    &ctx.config,
                    origin,
                    &doc,
                )
                .await;
            }
            let (origin_id, target_id) =
                ids_for(&ctx.db, origin, target).await.inspect_err(|_| {
                    tracing::warn!(
                        origin,
                        target,
                        "verified Move but rows unresolvable; dropping"
                    )
                })?;

            // Serialize concurrent Moves from the same origin on an
            // advisory lock: the event insert (deduped on the
            // activity id) and the moved-marker land atomically.
            let mut tx = ctx.db.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(origin)
                .execute(&mut *tx)
                .await?;
            // A live event for the same pair (a different Move
            // activity, or an activity-less replay racing the first)
            // merges into it rather than violating the active-event
            // index.
            let live: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM migration_events \
                 WHERE source_account_id = $1 AND target_account_id = $2 \
                 AND state IN ('pending', 'publishing', 'migrating') \
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(origin_id)
            .bind(target_id)
            .fetch_optional(&mut *tx)
            .await?;
            let event_id: i64 = match live {
                Some(id) => id,
                None => {
                    sqlx::query_scalar(
                        "INSERT INTO migration_events \
                         (source_account_id, target_account_id, source_activity_id, state) \
                         VALUES ($1, $2, $3, 'migrating') \
                         ON CONFLICT (source_activity_id) \
                         WHERE source_activity_id IS NOT NULL \
                         DO UPDATE SET updated_at = now() \
                         RETURNING id",
                    )
                    .bind(origin_id)
                    .bind(target_id)
                    .bind(&dedupe_key)
                    .fetch_one(&mut *tx)
                    .await?
                }
            };
            sqlx::query(
                "UPDATE accounts SET moved_to_account_id = \
                 (SELECT id FROM accounts WHERE actor_id = $2), moved_at = now(), \
                 updated_at = now() \
                 WHERE actor_id = $1 AND domain IS NOT NULL",
            )
            .bind(origin)
            .bind(target)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            event_id
        }
    } else {
        match sqlx::query_scalar::<_, i64>(
            "SELECT id FROM migration_events WHERE source_activity_id = $1",
        )
        .bind(&dedupe_key)
        .fetch_optional(&ctx.db)
        .await?
        {
            Some(id) => id,
            None => {
                let (o, t) = ids_for(&ctx.db, origin, target).await?;
                live_event(&ctx.db, o, t).await?
            }
        }
    };
    let (origin_id, target_id) = ids_for(&ctx.db, origin, target).await?;
    migrate_followers_batch(
        ctx,
        Some(ap),
        event_id,
        origin_id,
        target_id,
        cursor,
        payload,
    )
    .await
}

/// Register this module's job handlers — inbound verification fetches
/// documents; outbound unfollow publishes Undo; `PublishMove` is the
/// durable publication backstop.
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: &ApContext) {
    registry.register(
        JobKind::MigrateFollowers,
        MigrateFollowersJob::new(ap.clone()),
    );
    registry.register(JobKind::PublishMove, PublishMoveJob::new(ap.clone()));
}
