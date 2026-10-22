//! Outbound pipeline — durable ledger + queue fan-out.
//!
//! Pubby's `OutboxProcessor` collects the recipient inboxes (followers
//! ∪ addressed inboxes, minus blocked domains) and calls our
//! [`DeliverFn`] once per inbox — that seam writes a `deliveries`
//! ledger row and enqueues `FederationDelivery` instead of performing
//! the HTTP POST inline. `async_delivery=false` keeps `publish`
//! enqueue-only, so outbound delivery survives restarts like every
//! other job.
//!
//! Relay fan-out rides the same seam: the first call for a public
//! activity also inserts one ledger row per `relays.inbox_url` (the
//! `UNIQUE (activity_id, inbox_url)` constraint makes both the ledger
//! and the enqueues idempotent).

use std::sync::Arc;

use pubby_sdk::handlers::DeliverFn;
use pubby_sdk::http::HttpClient;
use pubby_sdk::{audience, ActivityPubError};
use serde_json::{json, Value};
use sqlx::Row;

use crate::jobs::{JobContext, JobHandler, JobKind};
use crate::Result;

use super::actors::sync_block;
use super::{actors, ApContext};

/// Grace horizon (minutes) stamped on `deliveries.next_attempt_at`:
/// "the queue should have run this row by now". A `pending` row past
/// it means its `FederationDelivery` job was lost (enqueue failure or
/// a crash between the ledger insert and the enqueue) — the
/// [`FederationDeliverySweepJob`] re-enqueues it. The queue's own
/// backoff still schedules every real retry; this column is purely
/// the lost-job deadline the sweeper selects on.
const STALE_GRACE_MINUTES: i64 = 15;

/// Rows a single sweep run re-enqueues.
const SWEEP_BATCH: i64 = 200;

/// The `DeliverFn` bound on every per-user handler: per collected
/// inbox → `deliveries(pending)` row + `FederationDelivery` job.
/// Duplicate ledger pairs are impossible (unique constraint), so a
/// re-published activity never double-delivers.
pub fn deliver_fn(ctx: &Arc<super::ApCore>) -> DeliverFn {
    let ctx = ctx.clone();
    Arc::new(move |inbox_url: &str, activity: &Value| {
        let ctx = ctx.clone();
        let inbox_url = inbox_url.to_string();
        let activity = activity.clone();
        if let Err(e) = sync_block(enqueue_delivery(&ctx, &inbox_url, &activity)) {
            tracing::error!(error = %e, inbox = %inbox_url, "delivery enqueue failed");
        }
    })
}

/// Insert the ledger row for (`activity`, `inbox`) and enqueue the job.
/// `INSERT … ON CONFLICT DO NOTHING RETURNING` gives the dedupe: only a
/// newly-created row enqueues. `pub(crate)` so the account-deletion
/// purge can address inboxes pubby's fan-out doesn't collect (remote
/// accounts the user followed).
pub(crate) async fn enqueue_delivery(
    ctx: &super::ApCore,
    inbox_url: &str,
    activity: &Value,
) -> Result<()> {
    let activity_uri = activity
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if activity_uri.is_empty() {
        return Err(crate::Error::Internal(
            "deliver called for an activity without an id".to_string(),
        ));
    }
    let delivery_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO deliveries (activity_id, inbox_url, next_attempt_at) \
         SELECT id, $2, now() + $3::int * interval '1 minute' \
         FROM activities WHERE activity_id = $1 \
         ON CONFLICT ON CONSTRAINT uq_deliveries_activity_inbox DO NOTHING \
         RETURNING id",
    )
    .bind(activity_uri)
    .bind(inbox_url)
    .bind(STALE_GRACE_MINUTES)
    .fetch_optional(&ctx.db)
    .await?;

    // Relay fan-out rides the first per-inbox call: public activities
    // also get one ledger row per accepted relay. Idempotent via the
    // same unique constraint.
    if ctx.config.federation.allow_relays && audience::is_public(activity) {
        let relay_rows = sqlx::query(
            "INSERT INTO deliveries (activity_id, inbox_url, next_attempt_at) \
             SELECT a.id, r.inbox_url, now() + $3::int * interval '1 minute' \
             FROM activities a, relays r \
             WHERE a.activity_id = $1 AND r.state = 'accepted' AND r.inbox_url <> $2 \
             ON CONFLICT ON CONSTRAINT uq_deliveries_activity_inbox DO NOTHING \
             RETURNING id, inbox_url",
        )
        .bind(activity_uri)
        .bind(inbox_url)
        .bind(STALE_GRACE_MINUTES)
        .fetch_all(&ctx.db)
        .await?;
        for row in relay_rows {
            enqueue_one(
                ctx,
                row.get("id"),
                activity_uri,
                &row.get::<String, _>("inbox_url"),
            )
            .await?;
        }
    }

    if let Some(delivery_id) = delivery_id {
        enqueue_one(ctx, delivery_id, activity_uri, inbox_url).await?;
    }
    Ok(())
}

async fn enqueue_one(
    ctx: &super::ApCore,
    delivery_id: i64,
    activity_uri: &str,
    inbox_url: &str,
) -> Result<()> {
    ctx.dispatcher
        .enqueue(
            JobKind::FederationDelivery,
            json!({
                "delivery_id": delivery_id,
                "activity_id": activity_uri,
                "inbox_url": inbox_url,
            }),
        )
        .await?;
    Ok(())
}

/// Publish an activity through a local account's handler: stores the
/// activity record and fans out through [`deliver_fn`]. `username` is
/// the local sender (the resolved handler signs as them).
pub async fn publish(ctx: &ApContext, username: &str, activity: &Value) -> Result<Value> {
    let handler = ctx
        .multi
        .for_username(username)
        .map_err(|e| crate::Error::Internal(format!("resolver failed: {e}")))?
        .ok_or_else(|| crate::Error::NotFound(format!("actor {username}")))?;
    let activity = activity.clone();
    actors::sync_call(move || handler.publish_activity(&activity)).map_err(crate::Error::Federation)
}

/// Publish an actor `Update` after a profile change (epic 09): drop
/// the stale handler-LRU entry first (it caches `ActorConfig`),
/// re-resolve, inject the banner `image` pubby can't emit
/// (`ActorConfig` has no banner field), then let pubby store the
/// `activities` row, invalidate its doc-cache prefixes and fan out to
/// follower inboxes through [`deliver_fn`].
pub async fn publish_actor_update(ctx: &ApContext, username: &str) -> Result<Value> {
    actors::invalidate_handlers(&ctx.handlers, Some(username));
    let handler = ctx
        .multi
        .for_username(username)
        .map_err(|e| crate::Error::Internal(format!("resolver failed: {e}")))?
        .ok_or_else(|| crate::Error::NotFound(format!("actor {username}")))?;
    let mut doc = handler.get_actor_document();
    let actor = actors::load_local_actor(&ctx.core.db, username).await?;
    if let Some(actor) = actor {
        let banner = actor.banner_remote_url.or_else(|| {
            actor
                .banner_path
                .map(|k| crate::media::public_url(&ctx.core.base_url, &k))
        });
        if let Some(url) = banner {
            doc["image"] = json!({ "type": "Image", "url": url });
        }
    }
    actors::sync_call(move || handler.publish_actor_update(Some(&doc)))
        .map_err(crate::Error::Federation)
}

/// The `FederationDelivery` job handler: one signed POST per
/// (activity, inbox) pair, no pubby-level retries — the job backoff
/// owns retries and the ledger records the outcome.
pub struct FederationDeliveryJob {
    ctx: ApContext,
}

impl FederationDeliveryJob {
    /// Create the handler bound to `ctx`.
    pub fn new(ctx: ApContext) -> Self {
        FederationDeliveryJob { ctx }
    }
}

#[async_trait::async_trait]
impl crate::jobs::JobHandler for FederationDeliveryJob {
    async fn run(&self, ctx: &crate::jobs::JobContext, payload: &Value) -> Result<()> {
        let delivery_id = payload
            .get("delivery_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                crate::Error::Validation("delivery job lacks delivery_id".to_string())
            })?;
        let row = sqlx::query(
            "SELECT d.id, d.inbox_url, d.attempts, a.object, a.actor_id \
             FROM deliveries d JOIN activities a ON a.id = d.activity_id \
             WHERE d.id = $1 AND d.state = 'pending'",
        )
        .bind(delivery_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some(row) = row else {
            // Already settled (sent/failed) or the activity was deleted.
            return Ok(());
        };
        // Lease: push the lost-job deadline out while this attempt runs
        // so the sweep doesn't requeue a row a worker already owns.
        sqlx::query(
            "UPDATE deliveries SET next_attempt_at = now() + $2::int * interval '1 minute' \
             WHERE id = $1 AND state = 'pending'",
        )
        .bind(delivery_id)
        .bind(STALE_GRACE_MINUTES)
        .execute(&ctx.db)
        .await?;
        let inbox_url: String = row.get("inbox_url");
        let attempts: i32 = row.get("attempts");
        let activity: Value = row.get("object");
        let actor_id: String = row.get("actor_id");

        let pem: Option<String> = sqlx::query_scalar(
            "SELECT private_key_pem FROM accounts WHERE actor_id = $1 AND domain IS NULL",
        )
        .bind(&actor_id)
        .fetch_optional(&ctx.db)
        .await?
        .flatten();
        let Some(pem) = pem else {
            // The instance actor signs its own deliveries; a missing key
            // is unrecoverable — mark failed rather than retry forever.
            sqlx::query(
                "UPDATE deliveries SET state = 'failed', last_error = $2, \
                 attempts = attempts + 1, updated_at = now() WHERE id = $1",
            )
            .bind(delivery_id)
            .bind("signer account or private key missing")
            .execute(&ctx.db)
            .await?;
            return Ok(());
        };
        let private_key = pubby_sdk::crypto::keys::load_private_key(&pem)
            .map_err(|e| crate::Error::Internal(format!("bad actor key: {e}")))?;
        let key_id = format!("{actor_id}#main-key");
        let http = http_client(&self.ctx);
        let user_agent = self.ctx.core.config.federation.user_agent.clone();

        let attempt = actors::sync_call(move || {
            pubby_sdk::deliver_activity(
                &activity,
                &inbox_url,
                &key_id,
                &private_key,
                Some(&user_agent),
                &http,
            )
        });

        match attempt {
            Ok(status) if (200..300).contains(&status) => {
                sqlx::query(
                    "UPDATE deliveries SET state = 'sent', attempts = attempts + 1, \
                     delivered_at = now(), updated_at = now() WHERE id = $1",
                )
                .bind(delivery_id)
                .execute(&ctx.db)
                .await?;
                Ok(())
            }
            Ok(status) => {
                self.mark_retry(&ctx.db, delivery_id, attempts, &format!("HTTP {status}"))
                    .await
            }
            Err(e) if permanent_delivery_error(&e) => {
                sqlx::query(
                    "UPDATE deliveries SET state = 'failed', attempts = attempts + 1, \
                     last_error = $2, updated_at = now() WHERE id = $1",
                )
                .bind(delivery_id)
                .bind(e.to_string())
                .execute(&ctx.db)
                .await?;
                Ok(())
            }
            Err(e) => {
                self.mark_retry(&ctx.db, delivery_id, attempts, &e.to_string())
                    .await
            }
        }
    }
}

impl FederationDeliveryJob {
    /// Record the failed attempt; the last budgeted attempt flips the
    /// row to `failed` instead of returning `Err` (which would
    /// dead-letter an already-marked delivery).
    ///
    /// Ledger `attempts`/`next_attempt_at` track the envelope's own
    /// counter only loosely — the Redis backoff schedules the real
    /// retry; the columns' job is bookkeeping plus marking the
    /// lost-job deadline the sweep selects on.
    async fn mark_retry(
        &self,
        db: &sqlx::PgPool,
        delivery_id: i64,
        attempts: i32,
        error: &str,
    ) -> Result<()> {
        let max = self.ctx.core.config.jobs.max_attempts as i32;
        if attempts + 1 >= max {
            sqlx::query(
                "UPDATE deliveries SET state = 'failed', attempts = attempts + 1, \
                 last_error = $2, updated_at = now() WHERE id = $1",
            )
            .bind(delivery_id)
            .bind(error)
            .execute(db)
            .await?;
            Ok(())
        } else {
            sqlx::query(
                "UPDATE deliveries SET attempts = attempts + 1, last_error = $2, \
                 next_attempt_at = now() + $3::int * interval '1 minute', \
                 updated_at = now() WHERE id = $1",
            )
            .bind(delivery_id)
            .bind(error)
            .bind(STALE_GRACE_MINUTES)
            .execute(db)
            .await?;
            Err(crate::Error::Internal(format!("delivery failed: {error}")))
        }
    }
}

/// `JobKind::FederationDeliverySweep` — re-enqueue `deliveries` rows
/// still `pending` past `next_attempt_at`: their queue entry was lost
/// (enqueue failure, or a crash between the ledger insert and the
/// enqueue). The row's deadline is bumped *before* re-enqueueing so a
/// slow retry doesn't get swept twice; delivery remains idempotent
/// because the job settles on `state`. Runs on
/// `federation.delivery_sweep_schedule` (cron).
pub struct FederationDeliverySweepJob;

#[async_trait::async_trait]
impl JobHandler for FederationDeliverySweepJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let rows = sqlx::query(
            "UPDATE deliveries SET \
                 next_attempt_at = now() + $1::int * interval '1 minute', updated_at = now() \
             WHERE id IN ( \
                 SELECT id FROM deliveries \
                 WHERE state = 'pending' AND next_attempt_at < now() \
                 ORDER BY id LIMIT $2 \
             ) AND state = 'pending' \
             RETURNING id, inbox_url, \
                 (SELECT activity_id FROM activities a \
                  WHERE a.id = deliveries.activity_id) AS activity_uri",
        )
        .bind(STALE_GRACE_MINUTES)
        .bind(SWEEP_BATCH)
        .fetch_all(&ctx.db)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        tracing::info!(requeued = rows.len(), "sweeping stale pending deliveries");
        let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        for row in rows {
            let activity_uri: Option<String> = row.get("activity_uri");
            let Some(activity_uri) = activity_uri else {
                continue;
            };
            dispatcher
                .enqueue(
                    JobKind::FederationDelivery,
                    json!({
                        "delivery_id": row.get::<i64, _>("id"),
                        "activity_id": activity_uri,
                        "inbox_url": row.get::<String, _>("inbox_url"),
                    }),
                )
                .await?;
        }
        Ok(())
    }
}

/// A `pubby` `HttpClient` honoring `federation.allow_private_targets`
/// and the configured timeout.
pub fn http_client(ctx: &ApContext) -> HttpClient {
    let timeout =
        std::time::Duration::from_secs(ctx.core.config.federation.request_timeout_seconds);
    if ctx.core.config.federation.allow_private_targets {
        HttpClient::new_relaxed(timeout)
    } else {
        HttpClient::new(timeout)
    }
}

/// Errors that retries cannot fix: signing/key failures and SSRF
/// rejections fail immediately instead of burning the retry budget.
fn permanent_delivery_error(e: &ActivityPubError) -> bool {
    match e {
        ActivityPubError::InvalidValue(_) | ActivityPubError::SignatureVerification(_) => true,
        ActivityPubError::General(m) => m.contains("unsafe") || m.contains("private"),
        _ => false,
    }
}
