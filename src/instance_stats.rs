//! Instance statistics shared by the public instance entity and the
//! periodic pre-calculation job.

use std::time::Duration;

use serde_json::Value;
use sqlx::{PgPool, Row};

use crate::{
    cache::{Redis, RedisCache},
    jobs::{JobContext, JobHandler},
    Result,
};

/// Redis cache key for `(user_count, status_count, domain_count)`.
pub(crate) const STATS_CACHE_KEY: &str = "instance:stats";

/// TTL for the cached counts. The default refresh schedule runs every
/// three hours; the extra hour absorbs queueing and retry delay before
/// a request has to compute the counts inline.
pub(crate) const STATS_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Run the counts query directly (cache miss or Redis down).
pub(crate) async fn load_stats(db: &PgPool) -> Result<(i64, i64, i64)> {
    // `domain_count` counts only *established* remote actors — bare
    // stubs for unfetchable phantom domains don't inflate it.
    let established = crate::activitypub::storage::established_remote_clause("accounts");
    let row = sqlx::query(&format!(
        "SELECT \
            (SELECT count(*) FROM accounts a JOIN users u ON u.account_id = a.id \
             WHERE a.domain IS NULL AND a.suspended_at IS NULL) AS user_count, \
            (SELECT count(*) FROM statuses WHERE local) AS status_count, \
            (SELECT count(DISTINCT domain) FROM accounts \
             WHERE domain IS NOT NULL AND {established}) AS domain_count",
    ))
    .fetch_one(db)
    .await?;
    Ok((
        row.get::<i64, _>("user_count"),
        row.get::<i64, _>("status_count"),
        row.get::<i64, _>("domain_count"),
    ))
}

/// Recompute the counts and store them in the shared cache.
pub(crate) async fn refresh_stats(db: &PgPool, redis: &Redis) -> Result<(i64, i64, i64)> {
    let stats = load_stats(db).await?;
    RedisCache::new(redis.clone())
        .set(STATS_CACHE_KEY, &stats, STATS_TTL)
        .await?;
    Ok(stats)
}

/// `JobKind::InstanceStatsRefresh` — precompute the public instance
/// counters so `/api/v1/instance` serves them from Redis. Scheduled via
/// `instance.stats_refresh_schedule`; payload is ignored.
pub struct InstanceStatsRefreshJob;

#[async_trait::async_trait]
impl JobHandler for InstanceStatsRefreshJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let (user_count, status_count, domain_count) = refresh_stats(&ctx.db, &ctx.redis).await?;
        tracing::info!(
            user_count,
            status_count,
            domain_count,
            "instance stats refreshed"
        );
        Ok(())
    }
}

/// Register this module's job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    registry.register(
        crate::jobs::JobKind::InstanceStatsRefresh,
        InstanceStatsRefreshJob,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_refresh_defaults_to_every_three_hours_with_cache_overlap() {
        assert_eq!(
            crate::config::Config::default()
                .instance
                .stats_refresh_schedule
                .as_deref(),
            Some("0 */3 * * *")
        );
        assert!(STATS_TTL > Duration::from_secs(3 * 60 * 60));
    }
}
