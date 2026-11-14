//! Import publication drain (epic 31 §8): `ImportPublish` walks one
//! import's `publication_work` rows — deterministic activity ids plus
//! `activities.activity_id` dedupe keep delivery once-only across
//! retries and re-enqueues. Rows exist only for `federate` imports
//! and `public`/`unlisted` items.

use crate::{
    activitypub::{self, delivery, ApContext},
    jobs::{JobContext, JobHandler, JobKind},
    posts::{self, PostContext},
    Error, Result,
};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::Row;

/// Work rows drained per run before re-enqueueing.
const PUBLISH_BATCH: i64 = 50;

/// Terminal sessions never leave pending work: cancellation and
/// failure settle every pending row.
pub(crate) async fn fail_pending_publication(
    db: &sqlx::PgPool,
    import_id: i64,
    reason: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE publication_work SET state = 'failed', error = $2, updated_at = now() \
         WHERE import_id = $1 AND state = 'pending'",
    )
    .bind(import_id)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark one work row terminally failed.
async fn fail_work(db: &sqlx::PgPool, work_id: i64, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE publication_work SET state = 'failed', error = $2, \
         updated_at = now() WHERE id = $1 AND state = 'pending'",
    )
    .bind(work_id)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// `JobKind::ImportPublish` — publish one import's durable work.
pub struct ImportPublishJob {
    ap: ApContext,
}

impl ImportPublishJob {
    /// Create the handler bound to the AP context.
    pub fn new(ap: ApContext) -> Self {
        ImportPublishJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ImportPublishJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let import_id = payload
            .get("import_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("import_publish job lacks import_id".into()))?;
        let mut claim = ctx.db.begin().await?;
        let acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("yonder:import-publication:{import_id}"))
                .fetch_one(&mut *claim)
                .await?;
        if !acquired {
            return Ok(());
        }
        let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        let mut again = std::time::Duration::ZERO;
        let mut worked = false;

        // A cancelled/failed import never publishes; a paused one drains later.
        let import_state: Option<(String, bool)> =
            sqlx::query_as("SELECT state, cancel_requested FROM archive_imports WHERE id = $1")
                .bind(import_id)
                .fetch_optional(&ctx.db)
                .await?;
        match import_state {
            Some((ref s, cancel)) if s == "cancelled" || s == "failed" || cancel => {
                return fail_pending_publication(&ctx.db, import_id, "import cancelled").await;
            }
            // Paused imports drain from the completion enqueue after
            // resume — polling a paused session would requeue forever.
            Some((ref s, _)) if s == "paused" => return Ok(()),
            Some((ref s, _)) if s != "done" => {
                // Import still running — drain once it finishes (it
                // enqueues again at completion).
                dispatcher
                    .enqueue_delayed(
                        JobKind::ImportPublish,
                        json!({ "import_id": import_id }),
                        std::time::Duration::from_secs(60),
                    )
                    .await?;
                return Ok(());
            }
            None => return Ok(()),
            _ => {}
        }

        let rows = sqlx::query(
            "SELECT id, import_item_id, status_id, activity_type, activity_id, payload, attempts \
             FROM publication_work \
             WHERE import_id = $1 AND state = 'pending' \
               AND (next_retry_at IS NULL OR next_retry_at <= now()) \
             ORDER BY id LIMIT $2",
        )
        .bind(import_id)
        .bind(PUBLISH_BATCH)
        .fetch_all(&ctx.db)
        .await?;

        for row in rows {
            worked = true;
            let work_id: i64 = row.get("id");
            let status_id: i64 = row.get("status_id");
            let activity_type: String = row.get("activity_type");
            let activity_id: String = row.get("activity_id");
            let payload: Value = row.get("payload");
            let attempts: i32 = row.get("attempts");

            // Eligibility re-check per work row (contract §9.6): a
            // suspended destination pauses, a deleted/disabled one
            // fails the row, a mid-drain cancel stops the batch.
            let gate: Option<(bool, bool, bool)> = sqlx::query_as(
                "SELECT a.suspended_at IS NOT NULL, \
                        a.deleted_at IS NOT NULL OR a.disabled_at IS NOT NULL \
                        OR EXISTS(SELECT 1 FROM account_deletions d WHERE d.account_id = a.id \
                               AND d.state IN ('pending','running')), \
                        i.cancel_requested \
                 FROM archive_imports i JOIN accounts a ON a.id = i.account_id \
                 WHERE i.id = $1",
            )
            .bind(import_id)
            .fetch_optional(&ctx.db)
            .await?;
            match gate {
                Some((_, _, true)) => {
                    return fail_pending_publication(&ctx.db, import_id, "import cancelled").await;
                }
                None | Some((_, true, _)) => {
                    fail_work(&ctx.db, work_id, "destination account deleted").await?;
                    continue;
                }
                Some((true, _, _)) => {
                    // Pause: re-enqueue the drain later, row stays
                    // pending so publication resumes with the account.
                    sqlx::query(
                        "UPDATE publication_work SET next_retry_at = now() + interval '10 minutes', \
                         updated_at = now() WHERE id = $1",
                    )
                    .bind(work_id)
                    .execute(&ctx.db)
                    .await?;
                    again = again.max(std::time::Duration::from_secs(600));
                    continue;
                }
                _ => {}
            }
            let mut status_lock = ctx.db.begin().await?;
            let eligible: Option<bool> = sqlx::query_scalar(
                "SELECT s.local AND NOT s.tombstone AND s.visibility IN ('public','unlisted') \
                    AND s.account_id = (SELECT account_id FROM archive_imports WHERE id = $2) \
                    AND (s.reblog_of_id IS NULL OR EXISTS \
                        (SELECT 1 FROM statuses t WHERE t.id = s.reblog_of_id \
                         AND NOT t.tombstone AND t.visibility IN ('public','unlisted'))) \
                 FROM statuses s WHERE s.id = $1 FOR UPDATE OF s",
            )
            .bind(status_id)
            .bind(import_id)
            .fetch_optional(&mut *status_lock)
            .await?;
            if eligible != Some(true) {
                fail_work(
                    &ctx.db,
                    work_id,
                    "status is no longer eligible for publication",
                )
                .await?;
                continue;
            }

            let username = payload
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match self
                .publish_one(ctx, status_id, &activity_type, &activity_id, &username)
                .await
            {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE publication_work SET state = 'delivered', \
                         updated_at = now() WHERE id = $1 AND state = 'pending'",
                    )
                    .bind(work_id)
                    .execute(&ctx.db)
                    .await?;
                }
                Err(e) => {
                    let max = ctx.config.jobs.max_attempts as i32;
                    let attempts = attempts + 1;
                    if attempts >= max {
                        sqlx::query(
                            "UPDATE publication_work SET state = 'failed', attempts = $2, \
                             error = $3, updated_at = now() WHERE id = $1 AND state = 'pending'",
                        )
                        .bind(work_id)
                        .bind(attempts)
                        .bind(e.to_string())
                        .execute(&ctx.db)
                        .await?;
                    } else {
                        // Capped backoff; `next_retry_at` drives the
                        // delayed re-enqueue below.
                        let secs = crate::jobs::queue::backoff_seconds(
                            attempts as u32,
                            ctx.config.jobs.backoff_base_seconds,
                            ctx.config.jobs.backoff_max_seconds,
                        );
                        sqlx::query(
                            "UPDATE publication_work SET attempts = $2, error = $3, \
                             next_retry_at = now() + ($4::bigint * interval '1 second'), \
                             updated_at = now() WHERE id = $1 AND state = 'pending'",
                        )
                        .bind(work_id)
                        .bind(attempts)
                        .bind(e.to_string())
                        .bind(secs as i64)
                        .execute(&ctx.db)
                        .await?;
                        again = again.max(std::time::Duration::from_secs(secs));
                    }
                }
            }
            status_lock.commit().await?;
        }

        // Duplicate envelopes are harmless: the transaction lock
        // excludes overlapping drains and every run rechecks due work.
        let (pending, next_due): (i64, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
            "SELECT count(*), min(next_retry_at) FROM publication_work \
             WHERE import_id = $1 AND state = 'pending'",
        )
        .bind(import_id)
        .fetch_one(&ctx.db)
        .await?;
        if pending > 0 {
            // Wait for the earliest due row — polling sooner only
            // burns drain jobs while every row sits in backoff.
            let delay = if worked && again.is_zero() {
                std::time::Duration::ZERO
            } else {
                next_due
                    .map(|t| (t - Utc::now()).to_std().unwrap_or_default())
                    .unwrap_or_default()
                    .max(again)
                    .max(std::time::Duration::from_secs(1))
            };
            dispatcher
                .enqueue_delayed(
                    JobKind::ImportPublish,
                    json!({ "import_id": import_id }),
                    delay,
                )
                .await?;
        }
        claim.commit().await?;
        Ok(())
    }
}

impl ImportPublishJob {
    /// Build the frozen activity and publish it. Republishing
    /// rebuilds missing `deliveries` rows (unique (activity, inbox)
    /// dedupes); a ledger-enqueue failure fails the attempt so the
    /// row retries instead of marking partial fan-out delivered.
    async fn publish_one(
        &self,
        ctx: &JobContext,
        status_id: i64,
        activity_type: &str,
        activity_id: &str,
        username: &str,
    ) -> Result<()> {
        let post_ctx = PostContext::from_job(ctx, Some(self.ap.clone()));
        let activity = match activity_type {
            "Create" => {
                let status = posts::load_any(&ctx.db, status_id)
                    .await?
                    .ok_or_else(|| Error::NotFound("status".into()))?;
                let doc = activitypub::posts::build_object(&post_ctx, &status).await?;
                let obj = pubby_sdk::Object::from_json(&doc);
                let handler = activitypub::handler_for_username(&self.ap, username)?
                    .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
                let mut activity = activitypub::actors::sync_call(move || {
                    handler.outbox.build_create_activity(&obj)
                })
                .await;
                if let Some(object) = activity.get_mut("object") {
                    activitypub::posts::merge_object_extras(object, &doc);
                }
                // Deterministic id + the source's original date.
                activity["id"] = json!(activity_id);
                activity["published"] = json!(pubby_sdk::model::dt_to_iso(&status.created_at));
                activity
            }
            "Announce" => {
                let (object_uri, visibility): (Option<String>, String) = sqlx::query_as(
                    "SELECT (SELECT uri FROM statuses WHERE id = s.reblog_of_id), s.visibility \
                     FROM statuses s WHERE s.id = $1",
                )
                .bind(status_id)
                .fetch_one(&ctx.db)
                .await?;
                let Some(object_uri) = object_uri else {
                    return Err(Error::NotFound("announce target".into()));
                };
                let actor_id: String = sqlx::query_scalar(
                    "SELECT actor_id FROM accounts WHERE id = \
                     (SELECT account_id FROM statuses WHERE id = $1)",
                )
                .bind(status_id)
                .fetch_one(&ctx.db)
                .await?;
                let handler = activitypub::handler_for_username(&self.ap, username)?
                    .ok_or_else(|| Error::NotFound(format!("actor {username}")))?;
                let published: Option<chrono::DateTime<chrono::Utc>> =
                    sqlx::query_scalar("SELECT created_at FROM statuses WHERE id = $1")
                        .bind(status_id)
                        .fetch_one(&ctx.db)
                        .await?;
                let aid = activity_id.to_string();
                let pub_v = published.map(|t| json!(pubby_sdk::model::dt_to_iso(&t)));
                let mut activity = activitypub::actors::sync_call(move || {
                    handler
                        .outbox
                        .build_announce_activity(&object_uri, Some(&aid), pub_v.as_ref())
                })
                .await;
                // Unlisted boosts must not be promoted to `as:Public`
                // by the builder's default audience.
                if visibility == "unlisted" {
                    activity["to"] = json!([format!("{actor_id}/followers")]);
                    activity["cc"] = json!([super::AS_PUBLIC]);
                }
                activity
            }
            other => {
                return Err(Error::Validation(format!(
                    "unsupported import activity {other:?}"
                )));
            }
        };
        // A ledger insert that fails during fan-out leaves the row
        // missing permanently (the sweeper only revives existing rows),
        // so this attempt is watched and retried until it lands whole.
        self.ap.core.delivery_watch.watch(activity_id);
        let published = delivery::publish(&self.ap, username, &activity).await;
        let ledger_failed = self.ap.core.delivery_watch.unwatch(activity_id);
        published?;
        if ledger_failed {
            return Err(Error::Internal(
                "delivery ledger enqueue failed during fan-out".into(),
            ));
        }
        Ok(())
    }
}
