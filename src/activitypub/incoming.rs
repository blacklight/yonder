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
use sqlx::PgPool;

use crate::jobs::{JobContext, JobHandler, JobKind};
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
            "Flag" => sync_block(handle_flag(&core.db, activity, raw)),
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

/// `reports.comment` CHECK limit (migration 0006).
const REPORT_COMMENT_MAX_CHARS: usize = 8000;

/// `Flag` — one `reports` row per resolvable object URL, reporter =
/// the remote actor. An object resolves to an existing
/// `accounts.actor_id` (→ `target_account_id`) or an existing
/// `statuses.uri` (→ the status's author + `status_id`); anything else
/// is skipped — objects are usually status URIs and fabricating
/// `accounts` rows for them would pollute the table.
pub async fn handle_flag(db: &PgPool, activity: &Activity, raw: &Value) -> Result<()> {
    let comment: String = raw
        .get("content")
        .or_else(|| raw.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(REPORT_COMMENT_MAX_CHARS)
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
        sqlx::query(
            "INSERT INTO reports (account_id, target_account_id, status_id, category, comment) \
             SELECT ra.id, $2, $3, 'other', $4 \
             FROM accounts ra WHERE ra.actor_id = $1",
        )
        .bind(&activity.actor)
        .bind(target_account_id)
        .bind(status_id)
        .bind(&comment)
        .execute(db)
        .await?;
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
pub struct RemoteActorFetchJob {
    ctx: ApContext,
}

impl RemoteActorFetchJob {
    /// Create the handler bound to `ctx`.
    pub fn new(ctx: ApContext) -> Self {
        RemoteActorFetchJob { ctx }
    }
}

#[async_trait::async_trait]
impl JobHandler for RemoteActorFetchJob {
    async fn run(&self, _ctx: &JobContext, payload: &Value) -> Result<()> {
        let actor_url = payload
            .get("actor_url")
            .or_else(|| payload.get("actor_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| crate::Error::Validation("actor fetch job lacks actor_url".to_string()))?
            .to_string();
        let handler = self.ctx.multi.instance.clone();
        let url = actor_url.clone();
        let doc = sync_call(move || handler.outbox.fetch_actor(&url));
        match doc {
            Some(doc) => {
                self.ctx
                    .core
                    .storage
                    .cache_remote_actor(&actor_url, &doc, None)
                    .map_err(crate::Error::Federation)?;
                super::storage::upsert_remote_actor_row(&self.ctx.core.db, &actor_url, &doc)
                    .await?;
                Ok(())
            }
            None => Err(crate::Error::Internal(format!(
                "remote actor fetch failed: {actor_url}"
            ))),
        }
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
