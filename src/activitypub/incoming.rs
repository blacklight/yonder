//! Inbound pipeline — the `FederationIncoming` worker, the
//! `on_activity` side-effect hook, `on_interaction_received` fan-out
//! and the `RemoteActorFetch` job.
//!
//! The route side ([`super::inbox`]) validated, deduped and enqueued;
//! the worker resolves the addressed handler
//! (`for_username`/`for_addressees` + the instance-actor fallback) and
//! runs pubby's `process_inbox_activity` — signature verification,
//! `on_activity`, then the built-in dispatch (Follow/Accept/Reject
//! settling, Undo, Create, Like, Announce, Delete, Update,
//! QuoteRequest) — inside `block_in_place` since every step is sync.
//!
//! `on_activity` is the seam for the activity types pubby doesn't
//! cover — `Move`, `Flag`, `Add`, `Remove`, `Block` land here and
//! answer `Handled`; everything pubby covers answers `PassThrough`
//! (hook errors also pass through rather than losing the activity).

use std::collections::HashMap;
use std::sync::Arc;

use pubby_sdk::handlers::inbox::{OnActivityReceived, OnInteractionReceived};
use pubby_sdk::{Activity, ActivityDisposition, ActivityPubStorage, Interaction, InteractionType};
use serde_json::{json, Value};

use crate::jobs::{Dispatcher, JobContext, JobHandler, JobKind, Queue};
use crate::Result;

use super::actors::{sync_block, sync_call};
use super::{ApContext, ApCore};

/// `JobKind::FederationIncoming` — verify and process one queued
/// inbound activity.
pub struct FederationIncomingJob {
    ctx: ApContext,
}

impl FederationIncomingJob {
    /// Create the handler bound to `ctx`.
    pub fn new(ctx: ApContext) -> Self {
        FederationIncomingJob { ctx }
    }
}

#[async_trait::async_trait]
impl JobHandler for FederationIncomingJob {
    async fn run(&self, _ctx: &JobContext, payload: &Value) -> Result<()> {
        let activity = payload
            .get("activity")
            .cloned()
            .ok_or_else(|| crate::Error::Validation("incoming job lacks activity".to_string()))?;
        let path = payload
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("/ap/inbox")
            .to_string();
        let headers: HashMap<String, String> = payload
            .get("headers")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let body = payload
            .get("body_hex")
            .and_then(|v| v.as_str())
            .and_then(|h| hex::decode(h).ok())
            .unwrap_or_default();

        let username = payload.get("username").and_then(|v| v.as_str());
        let handler = match username {
            Some(u) => match self.ctx.multi.for_username(u) {
                Ok(Some(h)) => h,
                // Unknown or suspended local actor — drop the activity.
                Ok(None) => return Ok(()),
                Err(e) => return Err(crate::Error::Internal(format!("resolver failed: {e}"))),
            },
            None => match self.ctx.multi.for_addressees(&activity) {
                Ok(h) => h,
                Err(e) => return Err(crate::Error::Internal(format!("resolver failed: {e}"))),
            },
        };

        let result = sync_call(move || {
            handler.process_inbox_activity(
                &activity,
                "POST",
                &path,
                Some(&headers),
                Some(&body),
                false,
            )
        });
        match result {
            Err(e) if e.is_signature_verification() => {
                // Signature failures never heal on retry — audit and
                // drop. The dedupe row is also released: it was written
                // before verification, so keeping it would permanently
                // shadow a correctly-signed redelivery of the same id
                // (e.g. after the remote rotates keys).
                tracing::warn!(error = %e, "inbound activity failed signature verification");
                let activity_id = activity_id_of(payload);
                if !activity_id.is_empty() {
                    sqlx::query(
                        "DELETE FROM activities \
                         WHERE activity_id = $1 AND NOT local AND processed_at IS NULL",
                    )
                    .bind(activity_id)
                    .execute(&self.ctx.core.db)
                    .await?;
                }
                Ok(())
            }
            Err(e) => Err(crate::Error::Federation(e)),
            Ok(_) => {
                let activity_id = activity_id_of(payload);
                if !activity_id.is_empty() {
                    sqlx::query(
                        "UPDATE activities SET processed_at = now() \
                         WHERE activity_id = $1 AND NOT local AND processed_at IS NULL",
                    )
                    .bind(activity_id)
                    .execute(&self.ctx.core.db)
                    .await?;
                }
                Ok(())
            }
        }
    }
}

/// The dedupe `activity_id` the inbox route stamped — payload or the
/// embedded `id`.
fn activity_id_of(payload: &Value) -> &str {
    payload
        .get("activity_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            payload
                .get("activity")
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
}

/// The `on_activity` hook bound on every handler: `Move`/`Flag`/
/// `Add`/`Remove`/`Block` are consumed here (`Handled`); everything
/// pubby dispatches natively passes through. Errors log and pass
/// through — a broken side-effect must not drop the activity.
pub fn on_activity_fn(core: &Arc<ApCore>) -> OnActivityReceived {
    let core = core.clone();
    Arc::new(move |activity: &Activity, raw: &Value| {
        // Create/Update/Delete of status objects materialize into
        // `statuses` rows (epic 12) — PassThrough keeps pubby's
        // interaction bookkeeping intact.
        if matches!(
            activity.activity_type.as_str(),
            "Create" | "Update" | "Delete"
        ) {
            // `handle` logs its own failures and never errors the
            // hook — PassThrough keeps pubby's dispatch intact.
            sync_block(crate::posts::remote::handle(&core, activity, raw));
            if activity.activity_type == "Update" {
                if let Err(e) = sync_block(handle_actor_update(&core, activity, raw)) {
                    tracing::warn!(error = %e, "actor Update side-effect failed");
                }
            }
            return Ok(ActivityDisposition::PassThrough);
        }
        // Interactions (epic 13): Like/Announce/EmojiReact normalize
        // into favourites/boost/reaction rows + counters; EmojiReact
        // answers Handled (pubby's dispatch ignores unknown types).
        if matches!(
            activity.activity_type.as_str(),
            "Like" | "Announce" | "EmojiReact" | "Undo" | "Accept"
        ) {
            return crate::interactions::inbound::handle(&core, activity, raw);
        }
        // Social graph (epic 14): `Follow` stays PassThrough (pubby
        // stores the follower/request — the hook just schedules the
        // notification settle); `Block` severs the local follow.
        if activity.activity_type == "Follow" {
            return crate::social::inbound::handle_follow(&core, activity, raw)
                .map_err(|e| pubby_sdk::ActivityPubError::General(e.to_string()));
        }
        if activity.activity_type == "Block" {
            return crate::social::inbound::handle_block(&core, activity, raw)
                .map_err(|e| pubby_sdk::ActivityPubError::General(e.to_string()));
        }
        let outcome = match activity.activity_type.as_str() {
            "Move" => sync_block(handle_move(&core, activity, raw)),
            "Flag" => sync_block(handle_flag(&core, activity, raw)),
            "Add" | "Remove" => sync_block(store_unhandled(&core.storage, activity, raw)),
            _ => return Ok(ActivityDisposition::PassThrough),
        };
        match outcome {
            Ok(()) => Ok(ActivityDisposition::Handled),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    activity_type = %activity.activity_type,
                    "on_activity side-effect failed; passing through"
                );
                Ok(ActivityDisposition::PassThrough)
            }
        }
    })
}

/// `Move` — an actor claims it migrated. Only the cheap checks run
/// inline (this callback is synchronous and network I/O isn't allowed
/// here): an actor may only move *itself* (`actor == object`), the
/// `target` must differ, and the origin must not be a local account
/// (local moves go through `/api/v1/yonder/move`). Everything else is
/// deferred to `MigrateFollowers`, which verifies the target's fresh
/// `alsoKnownAs` before migrating any local follower. **No** database
/// writes happen here — in particular no `account_aliases` rows: an
/// unverified `Move` must not alias a foreign actor onto a local
/// account.
pub async fn handle_move(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<()> {
    let extract_url = |field: &str| {
        raw.get(field)
            .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(|i| i.as_str())))
            .map(str::to_string)
    };
    // An actor may only move itself: `object` (the account moved) must
    // exist and equal the signer `actor`, and `target` must differ.
    // Signature verification already proved `activity.actor` is the
    // sender.
    let (Some(object_url), Some(target_url)) = (extract_url("object"), extract_url("target"))
    else {
        return Ok(());
    };
    if activity.actor.is_empty() || object_url != activity.actor || target_url == activity.actor {
        return Ok(());
    }
    let origin_is_local: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1 AND domain IS NULL)",
    )
    .bind(&activity.actor)
    .fetch_one(&core.db)
    .await?;
    if origin_is_local {
        return Ok(());
    }
    core.dispatcher
        .enqueue(
            JobKind::MigrateFollowers,
            json!({
                "direction": "remote",
                "origin_actor": activity.actor,
                "target_actor": target_url,
                "activity_id": activity.id,
            }),
        )
        .await?;
    Ok(())
}

/// `Update{Person}` — the activity carries the full actor document,
/// so a remote profile edit (fields, bio, avatar) refreshes the
/// `accounts` row immediately instead of waiting out the actor-cache
/// TTL. Ownership gate: `object.id` must equal the signer — anything
/// else is a spoof attempt and is dropped.
async fn handle_actor_update(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<()> {
    let object = raw.get("object").cloned().unwrap_or(Value::Null);
    let is_actor = object
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| crate::search::resolve::ACTOR_TYPES.contains(&t))
        .unwrap_or(false);
    if !is_actor {
        return Ok(());
    }
    if object.get("id").and_then(|v| v.as_str()) != Some(activity.actor.as_str()) {
        tracing::warn!(actor = %activity.actor, "spoofed actor Update dropped");
        return Ok(());
    }
    let account_id =
        super::storage::upsert_remote_actor_row(&core.db, &activity.actor, &object).await?;
    // An actor Update can change `attachment[]` — queue the `rel="me"`
    // pass so changed link fields get verified without waiting for a
    // profile read (unchanged fields keep their stamps via the upsert
    // carry-over).
    if let (Some(id), true) = (
        account_id,
        object
            .get("attachment")
            .and_then(|a| a.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false),
    ) {
        if let Err(e) =
            crate::users::profile::enqueue_verification(&core.redis, &core.config.jobs, id).await
        {
            tracing::warn!(actor = %activity.actor, error = %e, "profile-link verify enqueue failed");
        }
    }
    Ok(())
}

/// `Flag` — one `reports` row per resolvable object URL, reporter =
/// the remote actor. An object resolves to an existing
/// `accounts.actor_id` (→ `target_account_id`) or an existing
/// `statuses.uri` (→ the status's author + `status_id`); anything else
/// is skipped — objects are usually status URIs and fabricating
/// `accounts` rows for them would pollute the table. The reporter's
/// domain policy applies: `reject_reports` (or a full `suspend`)
/// drops the Flag silently. Admins/mods are notified per row.
pub async fn handle_flag(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<()> {
    let db = &core.db;
    if let Some(domain) = super::moderation::actor_domain(&activity.actor) {
        if crate::moderation::blocks::rejects_reports(db, &domain)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, domain, "reject_reports check failed; accepting");
                false
            })
        {
            tracing::info!(domain, "dropping inbound Flag: reject_reports");
            return Ok(());
        }
    }
    let comment: String = raw
        .get("content")
        .or_else(|| raw.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(std::cmp::min(
            core.config.content.max_report_comment as usize,
            crate::moderation::reports::COMMENT_MAX_CHARS,
        ))
        .collect();
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'') ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(super::moderation::actor_domain(&activity.actor))
    .bind(last_segment(&activity.actor))
    .bind(&activity.actor)
    .execute(db)
    .await?;

    for object_url in str_list(raw.get("object")) {
        let account_id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
                .bind(&object_url)
                .fetch_optional(db)
                .await?;
        let (target_account_id, status_id) = match account_id {
            Some(id) => (id, None),
            None => match sqlx::query_as::<_, (i64, i64)>(
                "SELECT id, account_id FROM statuses WHERE uri = $1",
            )
            .bind(&object_url)
            .fetch_optional(db)
            .await?
            {
                Some((status_id, author_id)) => (author_id, Some(status_id)),
                None => {
                    tracing::debug!(
                        object = %object_url,
                        "flag object not resolvable; skipped"
                    );
                    continue;
                }
            },
        };
        let report_id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO reports (account_id, target_account_id, status_id, category, comment) \
             SELECT ra.id, $2, $3, 'other', $4 \
             FROM accounts ra WHERE ra.actor_id = $1 \
             RETURNING id",
        )
        .bind(&activity.actor)
        .bind(target_account_id)
        .bind(status_id)
        .bind(&comment)
        .fetch_optional(db)
        .await?;
        if let Some(report_id) = report_id {
            // Best-effort — a notification failure must not drop the
            // stored report.
            if let Err(e) =
                crate::notifications::notify_report(db, &core.dispatcher, report_id).await
            {
                tracing::warn!(error = %e, report_id, "admin.report fan-out failed");
            }
        }
    }
    Ok(())
}

/// `Add`/`Remove`/`Block` — recorded as a generic interaction with the
/// real activity type in metadata; epics 13/21 own the semantics.
pub async fn store_unhandled(
    storage: &super::storage::PgActivityPubStorage,
    activity: &Activity,
    raw: &Value,
) -> Result<()> {
    let object_id = raw
        .get("object")
        .and_then(|v| v.as_str().or_else(|| v.get("id").and_then(|i| i.as_str())))
        .unwrap_or("")
        .to_string();
    let mut interaction = Interaction::new(
        &activity.actor,
        if object_id.is_empty() {
            &activity.actor
        } else {
            &object_id
        },
        InteractionType::Mention,
    );
    interaction.activity_id = activity.id.clone();
    interaction.object_id = object_id;
    interaction.metadata = serde_json::Map::from_iter([(
        "ap_type".to_string(),
        Value::String(activity.activity_type.clone()),
    )]);
    storage
        .store_interaction(&interaction)
        .map_err(crate::Error::Federation)
}

/// `on_interaction_received` — every stored interaction fans a
/// `NotificationFanout` job (epic 16 consumes). Best-effort: an
/// enqueue failure only logs — the interaction row is already durable.
pub fn on_interaction_fn(core: &Arc<ApCore>) -> OnInteractionReceived {
    let core = core.clone();
    Arc::new(move |interaction: &Interaction| {
        let core = core.clone();
        let payload = json!({
            "source_actor_id": interaction.source_actor_id,
            "target_resource": interaction.target_resource,
            "interaction_type": interaction.interaction_type.as_str(),
            "object_id": interaction.object_id,
            "activity_id": interaction.activity_id,
        });
        let outcome = sync_block(async {
            core.dispatcher
                .enqueue(JobKind::NotificationFanout, payload)
                .await
        });
        if let Err(e) = outcome {
            tracing::warn!(error = %e, "notification fanout enqueue failed");
        }
    })
}

/// `JobKind::RemoteActorFetch` — refresh a remote actor document into
/// `actor_cache` and upsert the `accounts` row. Uses the instance
/// actor's `fetch_actor` (signed fetch, SSRF-guarded).
///
/// Two payload shapes:
///
/// - `{"actor_url": "…"}` (or `actor_id`) — refresh one actor.
/// - `{}` — the `federation.remote_actor_refresh_schedule` sweep:
///   enqueue one fetch per remote account whose `actor_cache` row is
///   absent or older than [`REMOTE_ACTOR_STALE_SECONDS`]. Catches
///   profile edits whose `Update` activity never arrived.
pub struct RemoteActorFetchJob {
    ctx: ApContext,
}

/// Per-run cap on the stale-actor sweep — one fan-out must not flood
/// the queue unboundedly; the next firing picks up the remainder.
const ACTOR_SWEEP_BATCH_CAP: i64 = 1000;

impl RemoteActorFetchJob {
    /// Create the handler bound to `ctx`.
    pub fn new(ctx: ApContext) -> Self {
        RemoteActorFetchJob { ctx }
    }

    /// Sweep mode: fan out one `RemoteActorFetch` per stale remote
    /// account. Per-actor jobs (rather than inline fetches) keep
    /// retry/dead-letter accounting and parallelism; `Bulk` keeps the
    /// sweep off the interactive queue. Actors with a fresh
    /// failure/gone marker are skipped — the fetch job stamps them so
    /// the sweep can't respawn a retry cycle on every firing.
    async fn sweep(&self, ctx: &JobContext) -> Result<()> {
        let actor_ids: Vec<String> = sqlx::query_scalar(
            "SELECT a.actor_id FROM accounts a \
             LEFT JOIN actor_cache c ON c.actor_id = a.actor_id \
             WHERE a.domain IS NOT NULL AND a.deleted_at IS NULL \
               AND a.suspended_at IS NULL \
               AND (c.fetched_at IS NULL \
                    OR c.fetched_at < now() - ($1::float8 * interval '1 second')) \
               AND NOT EXISTS ( \
                    SELECT 1 FROM remote_network_syncs s \
                    WHERE s.account_id = a.id AND ( \
                        (s.direction = 'actor' \
                         AND s.synced_at > now() - ($1::float8 * interval '1 second')) \
                        OR (s.direction = 'actor_gone' \
                         AND s.synced_at > now() - ($2::float8 * interval '1 second')) \
                    )) \
             ORDER BY a.id LIMIT $3",
        )
        .bind(super::REMOTE_ACTOR_STALE_SECONDS as f64)
        .bind(super::GONE_RECHECK_SECONDS as f64)
        .bind(ACTOR_SWEEP_BATCH_CAP)
        .fetch_all(&ctx.db)
        .await?;
        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        for actor_id in actor_ids {
            dispatcher
                .enqueue_on(
                    Queue::Bulk,
                    JobKind::RemoteActorFetch,
                    json!({ "actor_url": actor_id }),
                )
                .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl JobHandler for RemoteActorFetchJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(actor_url) = payload
            .get("actor_url")
            .or_else(|| payload.get("actor_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return self.sweep(ctx).await;
        };
        match super::lookup::fetch_remote_object_detailed(&self.ctx, &actor_url).await? {
            super::lookup::RemoteFetch::Doc(fetched) => {
                let doc = fetched.doc;
                // Resolve a `movedTo` target we don't know yet before
                // the upsert — its move sync needs the target row (and
                // the fresh `alsoKnownAs`) to mark the move.
                ensure_move_target(&self.ctx, &actor_url, &doc).await;
                self.ctx
                    .core
                    .storage
                    .cache_remote_actor(&actor_url, &doc, None)
                    .map_err(crate::Error::Federation)?;
                let account_id =
                    super::storage::upsert_remote_actor_row(&self.ctx.core.db, &actor_url, &doc)
                        .await?;
                // Fields carried by the doc get a `rel="me"`
                // verification pass — fresh stamps land on profile
                // reads; unchanged fields keep theirs via the upsert
                // carry-over.
                if let (Some(id), true) = (
                    account_id,
                    doc.get("attachment")
                        .and_then(|a| a.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false),
                ) {
                    if let Err(e) = crate::users::profile::enqueue_verification(
                        &ctx.redis,
                        &ctx.config.jobs,
                        id,
                    )
                    .await
                    {
                        tracing::warn!(actor = %actor_url, error = %e, "profile-link verify enqueue failed");
                    }
                }
                refresh_remote_counters(&self.ctx, &actor_url, &doc).await;
                clear_fetch_markers(&self.ctx.core.db, &actor_url).await;
                Ok(())
            }
            super::lookup::RemoteFetch::Gone => {
                // The remote says the actor doesn't exist (or the URL
                // is unfetchable) — a retry can't help. Stamp the
                // markers so reads and the sweep stop re-enqueueing,
                // and ack the job instead of dead-lettering it.
                tracing::info!(actor = %actor_url, "remote actor gone; marking");
                stamp_fetch_markers(&self.ctx.core.db, &actor_url, &["actor", "actor_gone"]).await;
                Ok(())
            }
            super::lookup::RemoteFetch::Failed => {
                // Stamp the attempt so reads can't pile fresh jobs on
                // top of this one's retry cycle.
                stamp_fetch_markers(&self.ctx.core.db, &actor_url, &["actor"]).await;
                Err(crate::Error::Internal(format!(
                    "remote actor fetch failed: {actor_url}"
                )))
            }
        }
    }
}

/// Stamp `remote_network_syncs` markers for `actor_id` — the
/// `(account_id, direction)` timestamp doubles as the "last attempt"
/// record profile reads and the stale-actor sweep consult before
/// re-enqueueing this job. Best-effort: a failed stamp just means the
/// next read may enqueue again.
async fn stamp_fetch_markers(db: &sqlx::PgPool, actor_id: &str, directions: &[&str]) {
    for direction in directions {
        if let Err(e) = sqlx::query(
            "INSERT INTO remote_network_syncs (account_id, direction, synced_at) \
             SELECT id, $2, now() FROM accounts WHERE actor_id = $1 \
             ON CONFLICT (account_id, direction) DO UPDATE SET synced_at = now()",
        )
        .bind(actor_id)
        .bind(direction)
        .execute(db)
        .await
        {
            tracing::warn!(actor = %actor_id, direction = %direction, error = %e, "fetch marker stamp failed");
        }
    }
}

/// Drop the failure/gone markers after a successful fetch — a
/// resurrected actor resumes the normal refresh cadence.
async fn clear_fetch_markers(db: &sqlx::PgPool, actor_id: &str) {
    if let Err(e) = sqlx::query(
        "DELETE FROM remote_network_syncs \
         WHERE direction IN ('actor', 'actor_gone') \
         AND account_id = (SELECT id FROM accounts WHERE actor_id = $1)",
    )
    .bind(actor_id)
    .execute(db)
    .await
    {
        tracing::warn!(actor = %actor_id, error = %e, "fetch marker clear failed");
    }
}

/// Fetch a `movedTo` target that isn't a local `accounts` row yet, so
/// the upsert's move sync can verify its `alsoKnownAs` and mark the
/// move — the target row is needed anyway to render the `moved`
/// banner. Best-effort: an unfetchable target leaves the move
/// unmarked until a later refresh resolves it.
async fn ensure_move_target(ctx: &ApContext, actor_id: &str, doc: &Value) {
    let Some(target) = super::storage::doc_str(doc, "movedTo") else {
        return;
    };
    if target == actor_id {
        return;
    }
    let known: bool =
        match sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1)")
            .bind(&target)
            .fetch_one(&ctx.core.db)
            .await
        {
            Ok(known) => known,
            Err(e) => {
                tracing::warn!(actor = %actor_id, error = %e, "movedTo target lookup failed");
                return;
            }
        };
    if known {
        return;
    }
    let handler = ctx.multi.instance.clone();
    let url = target.clone();
    let Some(target_doc) = sync_call(move || handler.outbox.fetch_actor(&url)) else {
        tracing::info!(actor = %actor_id, target = %target, "movedTo target fetch failed");
        return;
    };
    if let Err(e) =
        super::storage::upsert_remote_actor_row(&ctx.core.db, &target, &target_doc).await
    {
        tracing::warn!(actor = %actor_id, target = %target, error = %e, "movedTo target upsert failed");
    }
}

/// Best-effort counter refresh for a remote actor: Mastodon-style
/// actor documents carry no `followersCount`/`statusesCount`, so the
/// `followers`/`following`/`outbox` collections' `totalItems` are the
/// public source. Each collection is one signed GET; failures only
/// log, never fail the job.
async fn refresh_remote_counters(ctx: &ApContext, actor_id: &str, doc: &Value) {
    for (key, col) in [
        ("followers", "followers_count"),
        ("following", "following_count"),
        ("outbox", "statuses_count"),
    ] {
        let Some(uri) = super::lookup::actor_collection_uri(doc, key) else {
            continue;
        };
        let Ok(Some(coll)) = super::lookup::fetch_remote_object(ctx, &uri)
            .await
            .map(|o| o.map(|f| f.doc))
        else {
            continue;
        };
        let Some(total) = super::lookup::total_items(&coll) else {
            // Pleroma-style outboxes expose no `totalItems` — the
            // honest `statuses_count` is what we've indexed locally.
            // (A failed fetch still skips, so a transient outage
            // doesn't clobber a good remote value.)
            if key == "outbox" {
                if let Err(e) = sqlx::query(
                    "UPDATE accounts SET statuses_count = ( \
                         SELECT count(*)::int FROM statuses s \
                         WHERE s.account_id = accounts.id AND NOT s.tombstone) \
                     WHERE actor_id = $1",
                )
                .bind(actor_id)
                .execute(&ctx.core.db)
                .await
                {
                    tracing::warn!(actor = %actor_id, error = %e, "local posts count fallback failed");
                }
            }
            continue;
        };
        if let Err(e) = sqlx::query(&format!(
            "UPDATE accounts SET {col} = $2 WHERE actor_id = $1"
        ))
        .bind(actor_id)
        .bind(total as i32)
        .execute(&ctx.core.db)
        .await
        {
            tracing::warn!(actor = %actor_id, key, error = %e, "remote counter refresh failed");
        }
    }
    // Stamp the marker so profile reads stop re-enqueueing this job
    // for the TTL — the attempt itself counts (a dead remote must not
    // re-trigger a refresh on every page view).
    if let Err(e) = sqlx::query(
        "INSERT INTO remote_network_syncs (account_id, direction, synced_at) \
         SELECT id, 'counters', now() FROM accounts WHERE actor_id = $1 \
         ON CONFLICT (account_id, direction) DO UPDATE SET synced_at = now()",
    )
    .bind(actor_id)
    .execute(&ctx.core.db)
    .await
    {
        tracing::warn!(actor = %actor_id, error = %e, "counters sync stamp failed");
    }
}

/// Last URL segment — the username guess used for bare remote rows.
fn last_segment(url: &str) -> &str {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url)
}

/// JSON string-list field → `Vec<String>` (single strings wrap).
fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}
