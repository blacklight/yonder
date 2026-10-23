//! Paginated timeline reads — the SQL half of every scope.
//!
//! Cursor semantics follow [`crate::http::paginate`]: `max_id` pages
//! older (`s.id < max`), `min_id`/`since_id` pages newer (`s.id >
//! since`). A `since_id` page runs `ASC` then reverses so the wire
//! order is always newest-first, matching Mastodon.

use sqlx::{postgres::PgArguments, query::Query, PgPool};

use crate::{
    http::paginate::Page,
    posts::{row_to_status, StatusRow, STATUS_COLS},
    Result,
};

use super::{
    filter::{build_where, Bind, Filters},
    scope::TimelineScope,
};

/// `STATUS_COLS` re-aliased for the joined query (`s.<col>`).
fn select_cols() -> String {
    STATUS_COLS
        .split(',')
        .map(|c| format!("s.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn bind_all<'a>(
    mut q: Query<'a, sqlx::Postgres, PgArguments>,
    binds: impl Iterator<Item = &'a Bind>,
) -> Query<'a, sqlx::Postgres, PgArguments> {
    for bind in binds {
        q = match bind {
            Bind::I64(v) => q.bind(*v),
            Bind::I64s(v) => q.bind(v.clone()),
            Bind::Text(v) => q.bind(v.clone()),
            Bind::Texts(v) => q.bind(v.clone()),
        };
    }
    q
}

/// Fetch one page of `scope`, newest-first. `page.max_id` reads
/// older; `page.since_id` reads the `limit` ids immediately newer and
/// reverses them back into descending order.
pub async fn fetch(
    db: &PgPool,
    scope: &TimelineScope,
    filters: &Filters,
    page: &Page,
) -> Result<Vec<StatusRow>> {
    let wh = build_where(scope, filters);
    let mut extra: Vec<Bind> = Vec::new();
    let mut suffix = String::new();
    let mut alloc = |bind: Bind| {
        extra.push(bind);
        format!("${}", wh.binds.len() + extra.len())
    };
    if let Some(max_id) = page.max_id {
        let p = alloc(Bind::I64(max_id));
        suffix.push_str(&format!(" AND s.id < {p}"));
    }
    let asc = page.since_id.is_some();
    if let Some(since_id) = page.since_id {
        let p = alloc(Bind::I64(since_id));
        suffix.push_str(&format!(" AND s.id > {p}"));
    }
    let limit = alloc(Bind::I64(page.limit));
    let sql = format!(
        "SELECT {} FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE {}{suffix} ORDER BY s.id {} LIMIT {limit}",
        select_cols(),
        wh.sql,
        if asc { "ASC" } else { "DESC" },
    );
    let rows = bind_all(sqlx::query(&sql), wh.binds.iter().chain(extra.iter()))
        .fetch_all(db)
        .await?;
    let mut statuses: Vec<StatusRow> = rows.iter().map(row_to_status).collect();
    if asc {
        statuses.reverse();
    }
    Ok(statuses)
}

/// Load the statuses in `ids` that still satisfy `scope` + `filters`,
/// in the order `ids` gave them. The Redis home index is validated
/// through here — stale members (unfollows, blocks, tombstones) drop
/// out of the page rather than leaking.
pub async fn fetch_by_ids(
    db: &PgPool,
    scope: &TimelineScope,
    filters: &Filters,
    ids: &[i64],
) -> Result<Vec<StatusRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let wh = build_where(scope, filters);
    let sql = format!(
        "SELECT {} FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE {} AND s.id = ANY(${})",
        select_cols(),
        wh.sql,
        wh.binds.len() + 1,
    );
    let rows = bind_all(
        sqlx::query(&sql),
        wh.binds.iter().chain([Bind::I64s(ids.to_vec())].iter()),
    )
    .fetch_all(db)
    .await?;
    let mut statuses: Vec<StatusRow> = rows.iter().map(row_to_status).collect();
    // `= ANY` doesn't preserve order — restore the index's order
    // (ids arrive newest-first by construction).
    statuses.sort_by_key(|s| ids.iter().position(|id| *id == s.id).unwrap_or(usize::MAX));
    Ok(statuses)
}

/// The newest `limit` home-feed ids for `filters.viewer` — the SQL
/// source `fanout::repopulate` rebuilds a cold index from.
pub async fn home_feed_ids(db: &PgPool, filters: &Filters, limit: i64) -> Result<Vec<i64>> {
    let page = Page {
        max_id: None,
        since_id: None,
        limit,
    };
    Ok(fetch(db, &TimelineScope::Home, filters, &page)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect())
}

/// The status ids of a page, fed to `paginate::headers` as cursors.
pub fn cursors(rows: &[StatusRow]) -> Vec<i64> {
    rows.iter().map(|s| s.id).collect()
}
