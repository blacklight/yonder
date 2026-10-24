//! `reports` — filing, admin listing and resolution (epic 21).
//!
//! Row granularity: one `reports` row per reported status, or one
//! account-level row when no `status_ids` are given (the schema's
//! `status_id` is single-valued). Mastodon clients filing N statuses
//! get one response entity but N rows — the admin listing groups by
//! (reporter, target, created day) when epic 22's panel needs it.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::http::AppState;
use crate::{Error, Result};

/// A `reports` row.
#[derive(Debug)]
pub struct ReportRow {
    pub id: i64,
    pub account_id: i64,
    pub target_account_id: i64,
    pub status_id: Option<i64>,
    pub category: String,
    pub comment: String,
    pub forward: bool,
    pub action_taken_at: Option<DateTime<Utc>>,
    pub assigned_account_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

const REPORT_COLS: &str = "id, account_id, target_account_id, status_id, category, comment, \
     forward, action_taken_at, assigned_account_id, created_at";

/// `reports.comment` CHECK ceiling (migration 0006). The effective
/// cap is `min(config.content.max_report_comment, COMMENT_MAX_CHARS)`
/// — a config value above the CHECK fails inserts with a 500, so both
/// callers clamp.
pub const COMMENT_MAX_CHARS: usize = 8000;

fn report_row(r: &sqlx::postgres::PgRow) -> ReportRow {
    ReportRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        target_account_id: r.get("target_account_id"),
        status_id: r.get("status_id"),
        category: r.get("category"),
        comment: r.get("comment"),
        forward: r.get("forward"),
        action_taken_at: r.get("action_taken_at"),
        assigned_account_id: r.get("assigned_account_id"),
        created_at: r.get("created_at"),
    }
}

/// File a report: one row per status (each verified to belong to the
/// target), or one account-level row. Runs inside `tx` so a
/// multi-status filing is all-or-nothing. Returns the primary row —
/// the entity response reports it.
pub async fn file(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    reporter_id: i64,
    target_account_id: i64,
    status_ids: &[i64],
    comment: &str,
    category: &str,
    forward: bool,
) -> Result<ReportRow> {
    if reporter_id == target_account_id {
        return Err(Error::Validation("cannot report yourself".into()));
    }
    if !matches!(category, "spam" | "violation" | "legal" | "other") {
        return Err(Error::Validation(format!("invalid category {category:?}")));
    }
    // Every reported status must belong to the target account.
    for sid in status_ids {
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM statuses WHERE id = $1 AND account_id = $2)",
        )
        .bind(sid)
        .bind(target_account_id)
        .fetch_one(&mut **tx)
        .await?;
        if !owned {
            return Err(Error::NotFound("status".into()));
        }
    }
    let mut primary = None;
    let ids: Vec<Option<i64>> = if status_ids.is_empty() {
        vec![None]
    } else {
        status_ids.iter().map(|s| Some(*s)).collect()
    };
    for status_id in ids {
        let row = sqlx::query(&format!(
            "INSERT INTO reports (account_id, target_account_id, status_id, category, comment, forward) \
             VALUES ($1,$2,$3,$4,$5,$6) RETURNING {REPORT_COLS}"
        ))
        .bind(reporter_id)
        .bind(target_account_id)
        .bind(status_id)
        .bind(category)
        .bind(comment)
        .bind(forward)
        .fetch_one(&mut **tx)
        .await?;
        primary.get_or_insert_with(|| report_row(&row));
    }
    Ok(primary.expect("at least one report row"))
}

/// Load one report.
pub async fn get(db: &PgPool, id: i64) -> Result<ReportRow> {
    let row = sqlx::query(&format!("SELECT {REPORT_COLS} FROM reports WHERE id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| Error::NotFound("report".into()))?;
    Ok(report_row(&row))
}

/// Admin listing: open reports by default (Mastodon's `resolved`
/// absent/false → unresolved only; `resolved=true` → resolved only).
/// Keyset pagination on id.
pub async fn list(
    db: &PgPool,
    resolved: Option<bool>,
    account_id: Option<i64>,
    target_account_id: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<ReportRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {REPORT_COLS} FROM reports \
         WHERE (action_taken_at IS NOT NULL) = COALESCE($1::bool, false) \
           AND ($2::bigint IS NULL OR account_id = $2) \
           AND ($3::bigint IS NULL OR target_account_id = $3) \
           AND ($4::bigint IS NULL OR id < $4) \
           AND ($5::bigint IS NULL OR id > $5) \
         ORDER BY id DESC LIMIT $6"
    ))
    .bind(resolved)
    .bind(account_id)
    .bind(target_account_id)
    .bind(max_id)
    .bind(since_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(report_row).collect())
}

/// `action_taken_at = now()` — the report is handled. `Conflict` on
/// double-resolve (Mastodon answers 4xx to a resolved report).
/// Runs inside `tx` so the caller can commit it with its audit row.
pub async fn resolve(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    actor_account_id: i64,
) -> Result<ReportRow> {
    let row = sqlx::query(&format!(
        "UPDATE reports SET action_taken_at = now(), \
             assigned_account_id = COALESCE(assigned_account_id, $2), updated_at = now() \
         WHERE id = $1 AND action_taken_at IS NULL RETURNING {REPORT_COLS}"
    ))
    .bind(id)
    .bind(actor_account_id)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(r) => Ok(report_row(&r)),
        None if exists(&mut *tx, id).await? => {
            Err(Error::Conflict("report is already resolved".into()))
        }
        None => Err(Error::NotFound("report".into())),
    }
}

/// Clear `action_taken_at`. `Conflict` when not resolved.
/// Runs inside `tx` so the caller can commit it with its audit row.
pub async fn reopen(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> Result<ReportRow> {
    let row = sqlx::query(&format!(
        "UPDATE reports SET action_taken_at = NULL, updated_at = now() \
         WHERE id = $1 AND action_taken_at IS NOT NULL RETURNING {REPORT_COLS}"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    match row {
        Some(r) => Ok(report_row(&r)),
        None if exists(&mut *tx, id).await? => {
            Err(Error::Conflict("report is not resolved".into()))
        }
        None => Err(Error::NotFound("report".into())),
    }
}

/// `assign_to_self` / `unassign` — `assigned` is `Some(caller)` or `None`.
/// Runs inside `tx` so the caller can commit it with its audit row.
pub async fn assign(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
    assignee: Option<i64>,
) -> Result<ReportRow> {
    let row = sqlx::query(&format!(
        "UPDATE reports SET assigned_account_id = $2, updated_at = now() \
         WHERE id = $1 RETURNING {REPORT_COLS}"
    ))
    .bind(id)
    .bind(assignee)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound("report".into()))?;
    Ok(report_row(&row))
}

/// Whether report `id` exists — the distinguish-404-from-409 check
/// for update statements that return no row.
async fn exists(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM reports WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
}

/// Mastodon `Report`/`Admin::Report` entity (the shapes differ only
/// in that admin consumers also read `assigned_account`/`action_taken_at`,
/// which we always include).
pub async fn entity(state: &AppState, report: &ReportRow) -> Result<Value> {
    let ctx = crate::posts::PostContext::new(state);
    let base_url = ctx.base_url();
    let account_json = |id: i64| {
        let base_url = base_url.clone();
        async move {
            match crate::users::account::load(&state.db, id).await? {
                Some(a) => {
                    crate::users::account::account_entity(&state.db, &state.config(), &base_url, &a)
                        .await
                }
                None => Ok(json!(null)),
            }
        }
    };
    let target = account_json(report.target_account_id).await?;
    let reporter = account_json(report.account_id).await?;
    let assigned = match report.assigned_account_id {
        Some(id) => account_json(id).await?,
        None => json!(null),
    };
    Ok(json!({
        "id": report.id.to_string(),
        "action_taken": report.action_taken_at.is_some(),
        "action_taken_at": report.action_taken_at.map(|t| t.to_rfc3339()),
        "category": report.category,
        "comment": report.comment,
        "forwarded": report.forward,
        "created_at": report.created_at.to_rfc3339(),
        "account_id": report.account_id.to_string(),
        "account": reporter,
        "target_account": target,
        "status_id": report.status_id.map(|s| s.to_string()),
        "assigned_account": assigned,
    }))
}
