//! Audit-log reader for `GET /api/v1/admin/audit_log` (epic 22) —
//! the write path lives in `crate::moderation::audit` (epic 21); this
//! module only queries the `audit_log` table and serializes rows.
//!
//! Filters: `action` as a prefix (`report.` covers `report.resolve`
//! and friends), `actor_account_id`, `target_type`, `target_id` and
//! `created_at` bounds. Pagination reuses the house
//! `max_id`/`min_id`/`limit` keyset contract (`http::paginate`).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, QueryBuilder, Row};

use crate::{http::paginate::Page, Result};

/// Parsed `audit_log` filters — all optional.
#[derive(Debug, Default)]
pub struct Filters {
    /// Prefix match on the dotted action (`"report."`, `"account"`).
    pub action: Option<String>,
    /// Only actions by this admin/moderator.
    pub actor_account_id: Option<i64>,
    /// Only actions on this target kind (`report`, `account`, …).
    pub target_type: Option<String>,
    /// Only actions on this target row.
    pub target_id: Option<i64>,
    /// `created_at >= since`.
    pub since: Option<DateTime<Utc>>,
    /// `created_at <= until`.
    pub until: Option<DateTime<Utc>>,
}

/// Newest-first page of audit entries matching `filters` and `page`'s
/// keyset cursors.
pub async fn list(db: &PgPool, filters: &Filters, page: &Page) -> Result<Vec<Value>> {
    let mut qb = QueryBuilder::<sqlx::Postgres>::new(
        "SELECT l.id, l.actor_account_id, l.action, l.target_type, l.target_id, \
         l.details, l.created_at, a.username AS actor_username, a.domain AS actor_domain \
         FROM audit_log l LEFT JOIN accounts a ON a.id = l.actor_account_id WHERE TRUE",
    );
    if let Some(action) = &filters.action {
        qb.push(" AND l.action LIKE ")
            .push_bind(format!("{}%", escape_like(action)))
            .push(" ESCAPE '\\'");
    }
    if let Some(actor) = filters.actor_account_id {
        qb.push(" AND l.actor_account_id = ").push_bind(actor);
    }
    if let Some(target_type) = &filters.target_type {
        qb.push(" AND l.target_type = ").push_bind(target_type);
    }
    if let Some(target_id) = filters.target_id {
        qb.push(" AND l.target_id = ").push_bind(target_id);
    }
    if let Some(since) = filters.since {
        qb.push(" AND l.created_at >= ").push_bind(since);
    }
    if let Some(until) = filters.until {
        qb.push(" AND l.created_at <= ").push_bind(until);
    }
    if let Some(max_id) = page.max_id {
        qb.push(" AND l.id < ").push_bind(max_id);
    }
    if let Some(min_id) = page.since_id {
        qb.push(" AND l.id > ").push_bind(min_id);
    }
    qb.push(" ORDER BY l.id DESC LIMIT ").push_bind(page.limit);
    let rows = qb.build().fetch_all(db).await?;
    Ok(rows.iter().map(entity).collect())
}

/// Escape `LIKE` metacharacters so a prefix filter matches literally
/// (`action=_` must not match every row).
fn escape_like(prefix: &str) -> String {
    prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Serialize one joined row; `details` passes through verbatim for the
/// UI to render.
fn entity(row: &sqlx::postgres::PgRow) -> Value {
    let actor_id: Option<i64> = row.get("actor_account_id");
    let actor = actor_id.map(|id| {
        json!({
            "id": id.to_string(),
            "username": row.get::<String, _>("actor_username"),
            "domain": row.get::<Option<String>, _>("actor_domain"),
        })
    });
    json!({
        "id": row.get::<i64, _>("id").to_string(),
        "actor_account_id": actor_id.map(|id| id.to_string()),
        "actor_account": actor,
        "action": row.get::<String, _>("action"),
        "target_type": row.get::<String, _>("target_type"),
        "target_id": row.get::<Option<i64>, _>("target_id").map(|id| id.to_string()),
        "details": row.get::<Value, _>("details"),
        "created_at": row.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
    })
}
