//! Trending hashtags — windowed usage counts over anonymous-visible
//! statuses, Redis-cached (epic 20).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use crate::{cache::RedisCache, config::TimelinesConfig, Error, Result};

use super::entity::HistoryDay;

/// The `?period=` selector — a Yonder extension (Mastodon has none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    Week,
    Month,
}

impl Period {
    /// Window length in days.
    pub fn days(self) -> u32 {
        match self {
            Period::Day => 1,
            Period::Week => 7,
            Period::Month => 30,
        }
    }

    /// `?period=day|week|month`; invalid → 422. Callers default a
    /// missing/empty param to `Week` themselves.
    pub fn parse(raw: &str) -> Result<Option<Period>> {
        match raw {
            "day" => Ok(Some(Period::Day)),
            "week" => Ok(Some(Period::Week)),
            "month" => Ok(Some(Period::Month)),
            _ => Err(Error::Validation(format!("unknown trends period '{raw}'"))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Period::Day => "day",
            Period::Week => "week",
            Period::Month => "month",
        }
    }
}

/// One trending tag row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendRow {
    pub tag_id: i64,
    pub name: String,
    pub uses: i64,
    pub accounts: i64,
}

/// Anonymous-visible usage window shared by ranking and history:
/// public, non-tombstone, non-boost statuses, excluding suspended and
/// limited/timeline-removed domains (same rule the public scopes use).
/// `include_remote = false` additionally restricts to local authors.
fn base_from(include_remote: bool) -> String {
    let remote_clause = if include_remote {
        ""
    } else {
        " AND a.domain IS NULL"
    };
    format!(
        " FROM status_tags st JOIN tags t ON t.id = st.tag_id \
         JOIN statuses s ON s.id = st.status_id \
         JOIN accounts a ON a.id = s.account_id \
         WHERE s.visibility = 'public' AND NOT s.tombstone \
           AND s.reblog_of_id IS NULL{remote_clause} \
           AND NOT EXISTS (SELECT 1 FROM domain_blocks idb \
                           WHERE idb.domain = a.domain \
                             AND idb.action IN ('suspend', 'limit', 'remove_from_timeline'))"
    )
}

/// Ceiling on the cached ranking — the handler offset-pages inside
/// it, so `?offset=` values past the ceiling yield an empty page
/// rather than an ever-growing Redis value.
pub const TRENDS_RANKING_CEILING: i64 = 1000;

/// Ranked tags over `window_days`, truncated to
/// [`TRENDS_RANKING_CEILING`] — the caller offset-pages the result.
/// `min_accounts` is the distinct-authors floor; ordering is uses
/// desc, accounts desc, name.
async fn query(
    db: &PgPool,
    window_days: u32,
    include_remote: bool,
    min_accounts: i64,
) -> Result<Vec<TrendRow>> {
    let sql = format!(
        "SELECT t.id AS tag_id, t.name, COUNT(*)::bigint AS uses, \
                COUNT(DISTINCT s.account_id)::bigint AS accounts {} \
         AND s.created_at > now() - ($1::text || ' days')::interval \
         GROUP BY t.id, t.name \
         HAVING COUNT(DISTINCT s.account_id) >= $2 \
         ORDER BY uses DESC, accounts DESC, t.name \
         LIMIT $3",
        base_from(include_remote)
    );
    let rows = sqlx::query(&sql)
        .bind(window_days.to_string())
        .bind(min_accounts)
        .bind(TRENDS_RANKING_CEILING)
        .fetch_all(db)
        .await?;
    Ok(rows
        .iter()
        .map(|r| TrendRow {
            tag_id: r.get("tag_id"),
            name: r.get("name"),
            uses: r.get("uses"),
            accounts: r.get("accounts"),
        })
        .collect())
}

/// Cached entry point — key includes period and the local/remote
/// mode. `trends_cache_seconds` = 0 bypasses the cache. The cached
/// ranking is capped at [`TRENDS_RANKING_CEILING`] rows.
pub async fn trending(
    db: &PgPool,
    cache: &RedisCache,
    config: &TimelinesConfig,
    period: Period,
) -> Result<Vec<TrendRow>> {
    let window = period.days();
    let include_remote = config.trends_include_remote;
    let min_accounts = config.trends_min_accounts as i64;
    let db2 = db.clone();
    let load = move || async move { query(&db2, window, include_remote, min_accounts).await };
    if config.trends_cache_seconds == 0 {
        return load().await;
    }
    let key = format!(
        "trends:tags:{}:{}",
        period.as_str(),
        if include_remote { "all" } else { "local" }
    );
    cache
        .get_or_load(&key, Duration::from_secs(config.trends_cache_seconds), load)
        .await
}

/// Turn raw `(day, uses, accounts)` rows into a zero-filled,
/// newest-first `history` of `days` buckets.
fn fill_days(rows: &[(chrono::NaiveDate, i64, i64)], days: u32) -> Vec<HistoryDay> {
    let today = chrono::Utc::now().date_naive();
    (0..days as i64)
        .map(|i| {
            let d = today - chrono::Duration::days(i);
            let (uses, accounts) = rows
                .iter()
                .find(|(day, _, _)| *day == d)
                .map(|(_, u, a)| (*u, *a))
                .unwrap_or((0, 0));
            HistoryDay {
                day: d
                    .and_hms_opt(0, 0, 0)
                    .unwrap_or_default()
                    .and_utc()
                    .timestamp(),
                uses,
                accounts,
            }
        })
        .collect()
}

/// One tag's trailing `history` (7 or `days` buckets), over the same
/// anonymous-visible window trends use — `include_remote` matches the
/// trends query so lookup and ranking agree.
pub async fn history_for(
    db: &PgPool,
    tag_id: i64,
    days: u32,
    include_remote: bool,
) -> Result<Vec<HistoryDay>> {
    let sql = format!(
        "SELECT date_trunc('day', s.created_at)::date AS day, \
                COUNT(*)::bigint AS uses, \
                COUNT(DISTINCT s.account_id)::bigint AS accounts {} \
         AND t.id = $1 AND s.created_at > now() - ($2::text || ' days')::interval \
         GROUP BY day ORDER BY day DESC",
        base_from(include_remote)
    );
    let rows = sqlx::query(&sql)
        .bind(tag_id)
        .bind(days.to_string())
        .fetch_all(db)
        .await?;
    let raw: Vec<(chrono::NaiveDate, i64, i64)> = rows
        .iter()
        .map(|r| (r.get("day"), r.get("uses"), r.get("accounts")))
        .collect();
    Ok(fill_days(&raw, days))
}

/// Batched `history` for a set of tags — one grouped query, one
/// [`HistoryDay`] vec per input id (same order as `tag_ids`).
pub async fn history_for_many(
    db: &PgPool,
    tag_ids: &[i64],
    days: u32,
    include_remote: bool,
) -> Result<Vec<Vec<HistoryDay>>> {
    if tag_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT t.id AS tag_id, date_trunc('day', s.created_at)::date AS day, \
                COUNT(*)::bigint AS uses, \
                COUNT(DISTINCT s.account_id)::bigint AS accounts {} \
         AND t.id = ANY($1) AND s.created_at > now() - ($2::text || ' days')::interval \
         GROUP BY t.id, day",
        base_from(include_remote)
    );
    let rows = sqlx::query(&sql)
        .bind(tag_ids)
        .bind(days.to_string())
        .fetch_all(db)
        .await?;
    let mut per_tag: std::collections::HashMap<i64, Vec<(chrono::NaiveDate, i64, i64)>> =
        std::collections::HashMap::new();
    for r in &rows {
        per_tag.entry(r.get("tag_id")).or_default().push((
            r.get("day"),
            r.get("uses"),
            r.get("accounts"),
        ));
    }
    Ok(tag_ids
        .iter()
        .map(|id| fill_days(per_tag.get(id).map(Vec::as_slice).unwrap_or(&[]), days))
        .collect())
}
