//! The Redis home-feed index and its writers.
//!
//! Each home feed is a sorted set `{prefix}:timeline:home:{account_id}`
//! whose members and scores are both status ids (scores double as the
//! pagination cursor). Writers only ever `ZADD`/`ZREM` ids; every read
//! re-validates through [`query`] + [`filter`], so the index can hold
//! stale entries (unfollows, blocks, tombstones) without them leaking
//! into a page — and a Redis outage degrades to plain SQL.

use redis::{pipe, AsyncCommands};
use serde_json::json;
use sqlx::PgPool;

use crate::{
    cache::Redis,
    config::TimelinesConfig,
    http::paginate::Page,
    jobs::{Dispatcher, JobKind},
    posts::StatusRow,
    Result,
};

use super::{filter::Filters, query, scope::TimelineScope};

/// `{prefix}:timeline:home:{account_id}`.
pub fn home_key(redis: &Redis, account_id: i64) -> String {
    redis.key(&format!("timeline:home:{account_id}"))
}

/// Enqueue a `TimelineFanOut` for one status — best-effort by
/// contract: the status row is already durable, so an enqueue failure
/// is a warn, never a request error.
pub async fn enqueue(dispatcher: &Dispatcher, status_id: i64) {
    if let Err(e) = dispatcher
        .enqueue(JobKind::TimelineFanOut, json!({ "status_id": status_id }))
        .await
    {
        tracing::warn!(status_id, error = %e, "timeline fan-out enqueue failed");
    }
}

/// Enqueue a follow backfill — `follower`'s home index gets
/// `followee`'s recent statuses. Used on follow create and on an
/// inbound `Accept` settling a remote follow.
pub async fn enqueue_backfill(dispatcher: &Dispatcher, follower: i64, followee: i64) {
    enqueue_backfill_inner(dispatcher, follower, followee, None).await;
}

/// Delayed variant of [`enqueue_backfill`] — the `Accept` hook must
/// let pubby's own dispatch land the `follows` row first.
pub async fn enqueue_backfill_delayed(
    dispatcher: &Dispatcher,
    follower: i64,
    followee: i64,
    delay: std::time::Duration,
) {
    enqueue_backfill_inner(dispatcher, follower, followee, Some(delay)).await;
}

async fn enqueue_backfill_inner(
    dispatcher: &Dispatcher,
    follower: i64,
    followee: i64,
    delay: Option<std::time::Duration>,
) {
    let payload = json!({ "backfill_for": follower, "of": followee });
    let outcome = match delay {
        Some(delay) => {
            dispatcher
                .enqueue_delayed(JobKind::TimelineFanOut, payload, delay)
                .await
        }
        None => dispatcher.enqueue(JobKind::TimelineFanOut, payload).await,
    };
    if let Err(e) = outcome {
        tracing::warn!(follower, followee, error = %e, "timeline backfill enqueue failed");
    }
}

/// The author's local followers, batched. The author leads the list —
/// their own home feed includes their own posts.
async fn local_follower_ids(db: &PgPool, author_id: i64, batch_size: i64) -> Result<Vec<i64>> {
    let mut ids = vec![author_id];
    let mut after = 0i64;
    loop {
        let batch: Vec<i64> = sqlx::query_scalar(
            "SELECT f.account_id FROM follows f \
             JOIN accounts a ON a.id = f.account_id \
             WHERE f.target_account_id = $1 AND a.domain IS NULL \
               AND f.account_id > $2 \
             ORDER BY f.account_id LIMIT $3",
        )
        .bind(author_id)
        .bind(after)
        .bind(batch_size)
        .fetch_all(db)
        .await?;
        match batch.last() {
            Some(last) => {
                after = *last;
                ids.extend(batch);
            }
            None => break,
        }
    }
    Ok(ids)
}

/// The fan-out writer: `ZADD` the status into every local follower's
/// home index plus the index of each tag follower (epic 20 — followed
/// hashtags land in home), trimming each to `home_max_entries`.
/// Idempotent — re-adding a member is a no-op. Tag-follower ZADDs are
/// bounded by `fanout_batch_size`; readers beyond the batch see the
/// post through the SQL top-up instead.
pub async fn add(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    status_id: i64,
    author_id: i64,
) -> Result<()> {
    let mut followers = local_follower_ids(db, author_id, config.fanout_batch_size as i64).await?;
    // Epic-20 tag followers: local accounts following any tag on this
    // status. Bounded — a viral tag never blows the job up.
    let tag_followers: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT tf.account_id FROM status_tags st \
         JOIN statuses s ON s.id = st.status_id \
         JOIN tag_follows tf ON tf.tag_id = st.tag_id \
         JOIN accounts a ON a.id = tf.account_id \
         WHERE st.status_id = $1 AND a.domain IS NULL \
           AND s.visibility = 'public' LIMIT $2",
    )
    .bind(status_id)
    .bind(config.fanout_batch_size as i64)
    .fetch_all(db)
    .await?;
    followers.extend(tag_followers);
    let mut conn = redis.conn().await?;
    let trim_stop = -(config.home_max_entries.max(1) as isize) - 1;
    for chunk in followers.chunks(config.fanout_batch_size.max(1) as usize) {
        let mut pipe = pipe();
        for follower in chunk {
            let key = home_key(redis, *follower);
            pipe.zadd(key.clone(), status_id, status_id)
                .zremrangebyrank(key, 0, trim_stop)
                .ignore();
        }
        pipe.query_async::<()>(&mut *conn).await?;
    }
    Ok(())
}

/// `ZREM` one status out of every feed the author's followers own —
/// deletes and unboosts. Best-effort hygiene: the read path's
/// tombstone predicate is the real guarantee.
pub async fn remove(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    status_id: i64,
    author_id: i64,
) -> Result<()> {
    let followers = local_follower_ids(db, author_id, config.fanout_batch_size as i64).await?;
    let mut conn = redis.conn().await?;
    for chunk in followers.chunks(config.fanout_batch_size.max(1) as usize) {
        let mut pipe = pipe();
        for follower in chunk {
            pipe.zrem(home_key(redis, *follower), status_id).ignore();
        }
        pipe.query_async::<()>(&mut *conn).await?;
    }
    Ok(())
}

/// Best-effort wrapper for the delete call sites — a failed `ZREM`
/// must not abort a delete already committed.
pub async fn remove_quiet(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    status_id: i64,
    author_id: i64,
) {
    if let Err(e) = remove(db, redis, config, status_id, author_id).await {
        tracing::warn!(status_id, error = %e, "timeline index removal failed");
    }
}

/// Backfill `follower`'s home index with `followee`'s recent
/// statuses — bounded by `home_max_entries`, honoring the current
/// `show_reblogs` flag. `direct` statuses never backfill.
pub async fn backfill(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    follower_id: i64,
    followee_id: i64,
) -> Result<()> {
    let show_reblogs: bool = sqlx::query_scalar(
        "SELECT COALESCE((SELECT show_reblogs FROM follows \
         WHERE account_id = $1 AND target_account_id = $2), true)",
    )
    .bind(follower_id)
    .bind(followee_id)
    .fetch_one(db)
    .await?;
    let ids: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT s.id FROM statuses s \
         WHERE s.account_id = $1 AND NOT s.tombstone \
           AND s.visibility <> 'direct' {} \
         ORDER BY s.id DESC LIMIT $2",
        if show_reblogs {
            ""
        } else {
            "AND s.reblog_of_id IS NULL"
        }
    ))
    .bind(followee_id)
    .bind(config.home_max_entries as i64)
    .fetch_all(db)
    .await?;
    if ids.is_empty() {
        return Ok(());
    }
    let key = home_key(redis, follower_id);
    let mut conn = redis.conn().await?;
    let mut pipe = pipe();
    for id in &ids {
        pipe.zadd(key.clone(), *id, *id).ignore();
    }
    pipe.zremrangebyrank(key, 0, -(config.home_max_entries.max(1) as isize) - 1)
        .ignore();
    pipe.query_async::<()>(&mut *conn).await?;
    Ok(())
}

/// The `(max, min)` score bounds for `ZREVRANGEBYSCORE` /
/// `ZRANGEBYSCORE` — `(` makes the cursor exclusive.
fn score_window(page: &Page) -> (String, String) {
    let max = page
        .max_id
        .map(|id| format!("({id}"))
        .unwrap_or_else(|| "+inf".to_string());
    let min = page
        .since_id
        .map(|id| format!("({id}"))
        .unwrap_or_else(|| "-inf".to_string());
    (max, min)
}

/// Read a home page: ZSET window → SQL re-validation → SQL top-up.
///
/// The index answers the common case (recent first page); a
/// shortfall, missing key or Redis error falls back to
/// [`query::fetch`], which evaluates the identical predicate over
/// Postgres. A short page can mean evicted or never-fanned entries —
/// topping up from SQL keeps the page contract intact. The index is
/// opportunistic, not a completeness proof: entries whose fan-out
/// never ran (failed enqueue, follows that predate the index) are
/// absent until the next rebuild, which only triggers on a *missing*
/// key — an existing key with an empty window is left alone.
pub async fn read_home(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    filters: &Filters,
    page: &Page,
) -> Result<Vec<StatusRow>> {
    let Some(viewer) = filters.viewer else {
        return Ok(Vec::new());
    };
    let scope = TimelineScope::Home;

    let (ids, key_exists): (Vec<i64>, bool) = {
        let windowed = async {
            let mut conn = redis.conn().await?;
            let (max, min) = score_window(page);
            let key = home_key(redis, viewer);
            let ids: Vec<i64> = if page.since_id.is_some() {
                // Newest-N-above-cursor, like the SQL ASC path.
                // `ZRANGEBYSCORE` takes (min, max) — the window
                // tuple returns (max, min), so the order flips.
                let mut asc: Vec<i64> = conn
                    .zrangebyscore_limit(&key, &min[..], &max[..], 0, page.limit as isize)
                    .await?;
                asc.reverse();
                asc
            } else {
                conn.zrevrangebyscore_limit(&key, &max[..], &min[..], 0, page.limit as isize)
                    .await?
            };
            // Only bother with EXISTS on a miss — a hit implies it.
            let exists = ids.is_empty() && conn.exists(&key).await?;
            Ok::<_, crate::Error>((ids, exists))
        }
        .await;
        match windowed {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "home index read failed; SQL fallback");
                return query::fetch(db, &scope, filters, page).await;
            }
        }
    };
    if ids.is_empty() {
        let rows = query::fetch(db, &scope, filters, page).await?;
        if !key_exists {
            // A missing key is a cold index — rebuild so the next
            // read hits. An existing-but-empty window is just a
            // sparse or exhausted page; SQL already answered it.
            // The index must be filter-free — read-time filters
            // like `hidden_languages` don't belong in it.
            if let Err(e) = repopulate(db, redis, config, viewer).await {
                tracing::debug!(error = %e, "home index repopulate failed");
            }
        }
        return Ok(rows);
    }
    let rows = query::fetch_by_ids(db, &scope, filters, &ids).await?;
    if rows.len() as i64 >= page.limit {
        return Ok(rows);
    }
    query::fetch(db, &scope, filters, page).await
}

/// Rebuild `viewer`'s index from the newest `rebuild_page_size`
/// home-feed ids — language/visibility filters excluded on purpose
/// (see [`read_home`]).
async fn repopulate(
    db: &PgPool,
    redis: &Redis,
    config: &TimelinesConfig,
    viewer: i64,
) -> Result<()> {
    let filters = Filters {
        viewer: Some(viewer),
        ..Default::default()
    };
    let ids = query::home_feed_ids(db, &filters, config.rebuild_page_size as i64).await?;
    if ids.is_empty() {
        return Ok(());
    }
    let key = home_key(redis, viewer);
    let mut conn = redis.conn().await?;
    let mut pipe = pipe();
    for id in &ids {
        pipe.zadd(key.clone(), *id, *id).ignore();
    }
    pipe.zremrangebyrank(key, 0, -(config.home_max_entries.max(1) as isize) - 1)
        .ignore();
    pipe.query_async::<()>(&mut *conn).await?;
    Ok(())
}

/// The index's members newest-first — used by tests and diagnostics.
#[allow(dead_code)]
pub async fn index_ids(redis: &Redis, account_id: i64) -> Result<Vec<i64>> {
    let mut conn = redis.conn().await?;
    Ok(conn
        .zrevrange::<_, Vec<i64>>(home_key(redis, account_id), 0, -1)
        .await?)
}
