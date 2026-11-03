//! `CleanupRemoteActivities` — context-only remote status pruning.
//!
//! Remote rows materialized only as thread context (path or subtree
//! expansion, a resolved quote/pin fetch) accrue without any local
//! user ever reading them. The sweep deletes the ones past
//! `remote_context.retention_days` that nothing local references, in
//! `prune_batch`-sized rounds per run — bounded storage without ever
//! deleting data a local user interacted with.
//!
//! Deletes are **hard deletes, never tombstones**: a pruned row that
//! is referenced again later (selected through `/context`, pointed at
//! by a new reply, reached by a path walk) re-hydrates through the
//! normal ingest — tombstones are reserved for explicit `Delete`
//! activities, which mean "never resurrect".
//!
//! Eligibility (all must hold):
//! - remote (`local = false`), has a `uri`, not tombstoned;
//! - stored longer than `retention_days` (`fetched_at` — the origin's
//!   `published` in `created_at` can be years old for freshly fetched
//!   context), and not read through `/context` within the window
//!   (`last_selected_at`);
//! - no *local* anchor: no local favourite, bookmark, mention, pin,
//!   notification, emoji reaction, filter entry, report, conversation
//!   mute, or local vote on its poll, and its author isn't followed
//!   by a local account;
//! - not referenced by any kept row: no surviving status links to it
//!   (`in_reply_to_id`/`quote_of_id`/`reblog_of_id`) and no parked
//!   URI reference (`in_reply_to_uri`/`quote_uri`) names it. The
//!   keep-set is a *fixpoint*: a surviving reply retains its whole
//!   ancestor chain, not just the direct parent.
//!
//! Candidate scanning uses a persistent id cursor (Redis
//! `remote-prune-scan`): each sweep resumes where the last one left
//! off and wraps once at the end of the id space, so a permanently
//! blocked head of the table can't stall the sweep.
//!
//! Counter consistency: parent `replies_count`/`quotes_count` and the
//! author's `statuses_count` decrement inside the same statement —
//! matching the tombstone path's releases. Orphaned media rows
//! (`status_id` SET NULL) are reclaimed by `CleanupOrphanMedia`.

use serde_json::{json, Value};
use sqlx::PgPool;
use std::time::Duration;

use crate::cache::RedisCache;
use crate::jobs::{JobContext, JobHandler};
use crate::Result;

/// `JobKind::CleanupRemoteActivities` — `{}` sweeps context-only
/// remote statuses past the retention window. Scheduled via
/// `federation.remote_activity_prune_schedule`; runs on the `cleanup`
/// queue so a long sweep can't starve per-item fan-out work.
pub struct CleanupRemoteActivitiesJob;

/// Rows one sweep will not exceed — a hard safety cap on top of
/// `prune_batch` so a pathological run can't hold a long transaction.
const SWEEP_HARD_CAP: i64 = 10_000;
/// Candidate rows one sweep will *examine* — permanently blocked rows
/// are skipped, but they still cost the eligibility scan.
const SCAN_HARD_CAP: i64 = 20_000;
/// Redis key holding the scan frontier (`i64`) across sweeps.
const CURSOR_KEY: &str = "remote-prune-scan";

#[async_trait::async_trait]
impl JobHandler for CleanupRemoteActivitiesJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let cfg = &ctx.config.remote_context;
        let batch = (cfg.prune_batch as i64).min(SWEEP_HARD_CAP);
        let days = cfg.retention_days as i64;
        let cache = RedisCache::new(ctx.redis.clone());
        let cursor_key = ctx.redis.key(CURSOR_KEY);
        let mut after: i64 = cache
            .get(&cursor_key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let mut deleted_total = 0i64;
        let mut scanned_total = 0i64;
        let mut wrapped = false;
        loop {
            if deleted_total >= batch || scanned_total >= SCAN_HARD_CAP {
                break;
            }
            let round = prune_round(&ctx.db, days, after, batch).await?;
            scanned_total += round.scanned;
            deleted_total += round.deleted;
            if round.scanned < batch {
                // Reached the end of the id space — wrap once to pick
                // up rows below the frontier that became eligible.
                if wrapped {
                    break;
                }
                wrapped = true;
                after = 0;
                continue;
            }
            after = round.max_id;
        }
        let _ = cache
            .set(
                &cursor_key,
                &json!(after),
                Duration::from_secs(60 * 60 * 24 * 90),
            )
            .await;
        if deleted_total > 0 {
            tracing::info!(
                deleted = deleted_total,
                scanned = scanned_total,
                "remote activity prune sweep done"
            );
        }
        Ok(())
    }
}

/// One round's counters.
struct PruneRound {
    /// Eligible candidates examined (before reference blocking).
    scanned: i64,
    /// Highest candidate id seen — the scan frontier.
    max_id: i64,
    /// Rows actually deleted.
    deleted: i64,
}

/// One bounded delete round — examines up to `limit` candidates with
/// `id > after`, deletes those not retained by the keep-set fixpoint,
/// and returns the round's counters. Everything happens in one
/// statement so the keep-set/delete-set split is atomic and the
/// counter adjustments ride the same transaction.
async fn prune_round(
    db: &PgPool,
    retention_days: i64,
    after: i64,
    limit: i64,
) -> Result<PruneRound> {
    // Reference checks — the eligibility exclusions and the keep-set
    // seed alike — are split into per-column `EXISTS` subqueries so
    // each maps to its own index (the `in_reply_to_uri`/`quote_uri`
    // arms add the NULL predicate the partial indexes require); a
    // single five-way OR inside one subquery would force a seq scan
    // per candidate. The `kept` CTE is recursive: the seed is
    // candidates referenced by non-candidates, and the recursion
    // climbs references *from* kept rows, so a surviving reply
    // retains its whole ancestor chain.
    let row: (i64, i64, i64) = sqlx::query_as(
        "WITH RECURSIVE candidates AS ( \
             SELECT s.id, s.uri FROM statuses s \
             WHERE s.id > $3 \
               AND NOT s.local AND s.uri IS NOT NULL AND NOT s.tombstone \
               AND s.fetched_at < now() - ($1::int * interval '1 day') \
               AND (s.last_selected_at IS NULL \
                    OR s.last_selected_at < now() - ($1::int * interval '1 day')) \
               AND NOT EXISTS (SELECT 1 FROM reports rep WHERE rep.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM conversation_mutes cm WHERE cm.root_status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM filter_statuses fs WHERE fs.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM notifications n WHERE n.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM favourites f \
                               JOIN accounts la ON la.id = f.account_id AND la.domain IS NULL \
                               WHERE f.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM bookmarks b \
                               JOIN accounts la ON la.id = b.account_id AND la.domain IS NULL \
                               WHERE b.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM status_mentions m \
                               JOIN accounts la ON la.id = m.account_id AND la.domain IS NULL \
                               WHERE m.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM status_pins p \
                               JOIN accounts la ON la.id = p.account_id AND la.domain IS NULL \
                               WHERE p.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM emoji_reactions e \
                               JOIN accounts la ON la.id = e.account_id AND la.domain IS NULL \
                               WHERE e.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM polls p \
                               JOIN poll_votes pv ON pv.poll_id = p.id \
                               JOIN accounts la ON la.id = pv.account_id AND la.domain IS NULL \
                               WHERE p.status_id = s.id) \
               AND NOT EXISTS (SELECT 1 FROM follows f \
                               JOIN accounts la ON la.id = f.account_id AND la.domain IS NULL \
                               WHERE f.target_account_id = s.account_id) \
             ORDER BY s.id LIMIT $2 \
         ), \
         kept(id) AS ( \
             SELECT c.id FROM candidates c \
             WHERE EXISTS (SELECT 1 FROM statuses r \
                           WHERE r.id NOT IN (SELECT id FROM candidates) \
                             AND r.in_reply_to_id = c.id) \
                OR EXISTS (SELECT 1 FROM statuses r \
                           WHERE r.id NOT IN (SELECT id FROM candidates) \
                             AND r.quote_of_id = c.id) \
                OR EXISTS (SELECT 1 FROM statuses r \
                           WHERE r.id NOT IN (SELECT id FROM candidates) \
                             AND r.reblog_of_id = c.id) \
                OR EXISTS (SELECT 1 FROM statuses r \
                           WHERE r.id NOT IN (SELECT id FROM candidates) \
                             AND r.in_reply_to_uri = c.uri \
                             AND r.in_reply_to_id IS NULL) \
                OR EXISTS (SELECT 1 FROM statuses r \
                           WHERE r.id NOT IN (SELECT id FROM candidates) \
                             AND r.quote_uri = c.uri \
                             AND r.quote_of_id IS NULL) \
             UNION \
             SELECT c.id FROM candidates c, statuses r, kept k \
             WHERE r.id = k.id \
               AND (r.in_reply_to_id = c.id \
                    OR r.quote_of_id = c.id \
                    OR r.reblog_of_id = c.id \
                    OR (r.in_reply_to_uri = c.uri AND r.in_reply_to_id IS NULL) \
                    OR (r.quote_uri = c.uri AND r.quote_of_id IS NULL)) \
         ), \
         del AS ( \
             DELETE FROM statuses \
             WHERE id IN (SELECT id FROM candidates \
                          WHERE id NOT IN (SELECT id FROM kept)) \
             RETURNING id, account_id, in_reply_to_id, quote_of_id \
         ), \
         rel AS ( \
             UPDATE statuses s SET replies_count = GREATEST(s.replies_count - d.n, 0) \
             FROM (SELECT in_reply_to_id AS pid, count(*) AS n FROM del \
                   WHERE in_reply_to_id IS NOT NULL GROUP BY 1) d \
             WHERE s.id = d.pid AND s.id NOT IN (SELECT id FROM del) \
         ), \
         qte AS ( \
             UPDATE statuses s SET quotes_count = GREATEST(s.quotes_count - d.n, 0) \
             FROM (SELECT quote_of_id AS qid, count(*) AS n FROM del \
                   WHERE quote_of_id IS NOT NULL GROUP BY 1) d \
             WHERE s.id = d.qid AND s.id NOT IN (SELECT id FROM del) \
         ), \
         act AS ( \
             UPDATE accounts a SET statuses_count = GREATEST(a.statuses_count - d.n, 0) \
             FROM (SELECT account_id AS aid, count(*) AS n FROM del GROUP BY 1) d \
             WHERE a.id = d.aid \
         ) \
         SELECT (SELECT count(*) FROM candidates) AS scanned, \
                (SELECT COALESCE(max(id), 0) FROM candidates) AS max_id, \
                (SELECT count(*) FROM del) AS deleted",
    )
    .bind(retention_days)
    .bind(limit)
    .bind(after)
    .fetch_one(db)
    .await?;
    Ok(PruneRound {
        scanned: row.0,
        max_id: row.1,
        deleted: row.2,
    })
}
