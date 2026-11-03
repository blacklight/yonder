//! Inline CSV/JSON exporters (epic 31 section 7): the Mastodon-shaped
//! per-kind files, plus `custom_filters.json`.

use super::*;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

// ---------------------------------------------------------------------------
// Export (inline kinds)
// ---------------------------------------------------------------------------

/// Generate a standalone CSV/JSON export for `account_id`. Returns
/// `(file_name, content_type, bytes)`.
pub async fn export_kind(
    db: &PgPool,
    local_domain: &str,
    account_id: i64,
    kind: Kind,
) -> Result<(String, &'static str, Vec<u8>)> {
    let out = match kind {
        Kind::Following => {
            let rows = sqlx::query_as::<_, (String, Option<String>, bool, bool)>(
                "SELECT a.username, a.domain, f.show_reblogs, f.notify \
                 FROM follows f JOIN accounts a ON a.id = f.target_account_id \
                 WHERE f.account_id = $1 ORDER BY a.id",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            w.write_record(["Account address", "Show boosts", "Notify on new posts", "Languages"])
                .map_err(csv_err)?;
            for (u, d, show, notify) in rows {
                w.write_record([
                    render_acct(&u, d.as_deref(), local_domain),
                    show.to_string(),
                    notify.to_string(),
                    String::new(),
                ])
                .map_err(csv_err)?;
            }
            (Kind::Following.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::Followers => {
            let rows = sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT a.username, a.domain \
                 FROM follows f JOIN accounts a ON a.id = f.account_id \
                 WHERE f.target_account_id = $1 ORDER BY a.id",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            w.write_record(["Account address"]).map_err(csv_err)?;
            for (u, d) in rows {
                w.write_record([render_acct(&u, d.as_deref(), local_domain)])
                    .map_err(csv_err)?;
            }
            (Kind::Followers.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::Blocking => {
            csv_export_accts(
                db,
                "SELECT a.username, a.domain FROM blocks b \
                 JOIN accounts a ON a.id = b.target_account_id WHERE b.account_id = $1 ORDER BY a.id",
                account_id,
                local_domain,
            )
            .await?
        }
        Kind::Muting => {
            let rows = sqlx::query_as::<_, (String, Option<String>, bool)>(
                "SELECT a.username, a.domain, m.hide_notifications FROM mutes m \
                 JOIN accounts a ON a.id = m.target_account_id WHERE m.account_id = $1 ORDER BY a.id",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            w.write_record(["Account address", "Hide notifications"])
                .map_err(csv_err)?;
            for (u, d, hide) in rows {
                w.write_record([render_acct(&u, d.as_deref(), local_domain), hide.to_string()])
                    .map_err(csv_err)?;
            }
            (Kind::Muting.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::DomainBlocks => {
            let domains = sqlx::query_scalar::<_, String>(
                "SELECT domain FROM account_domain_blocks WHERE account_id = $1 ORDER BY domain",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            for d in domains {
                w.write_record([d]).map_err(csv_err)?;
            }
            (Kind::DomainBlocks.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::Bookmarks => {
            let uris = sqlx::query_scalar::<_, String>(
                "SELECT s.uri FROM bookmarks b JOIN statuses s ON s.id = b.status_id \
                 WHERE b.account_id = $1 AND s.uri IS NOT NULL ORDER BY b.id",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            for u in uris {
                w.write_record([u]).map_err(csv_err)?;
            }
            (Kind::Bookmarks.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::Lists => {
            let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT l.title, a.username, a.domain FROM list_accounts la \
                 JOIN lists l ON l.id = la.list_id JOIN accounts a ON a.id = la.account_id \
                 WHERE l.account_id = $1 ORDER BY l.title, a.id",
            )
            .bind(account_id)
            .fetch_all(db)
            .await?;
            let mut w = ::csv::Writer::from_writer(vec![]);
            for (t, u, d) in rows {
                w.write_record([t, render_acct(&u, d.as_deref(), local_domain)])
                    .map_err(csv_err)?;
            }
            (Kind::Lists.file_name(), "text/csv", w.into_inner().map_err(csv_err)?)
        }
        Kind::Filters => {
            let bytes = export_filters(db, account_id).await?;
            (Kind::Filters.file_name(), "application/json", bytes)
        }
        Kind::Archive => {
            return Err(Error::Validation(
                "the archive export is asynchronous — POST with kind=archive".into(),
            ))
        }
    };
    Ok((out.0.to_string(), out.1, out.2))
}

pub(crate) fn csv_err(e: impl std::fmt::Display) -> Error {
    Error::Internal(format!("csv error: {e}"))
}

/// Single-column acct CSV for `Blocking`-shaped kinds.
pub(crate) async fn csv_export_accts(
    pool: &PgPool,
    sql: &str,
    account_id: i64,
    local_domain: &str,
) -> Result<(&'static str, &'static str, Vec<u8>)> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(sql)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    let mut w = ::csv::Writer::from_writer(vec![]);
    for (u, d) in rows {
        w.write_record([render_acct(&u, d.as_deref(), local_domain)])
            .map_err(csv_err)?;
    }
    Ok((
        Kind::Blocking.file_name(),
        "text/csv",
        w.into_inner().map_err(csv_err)?,
    ))
}

/// The `custom_filters.json` export — Mastodon's shape
/// (`filter_action`, `keywords_attributes`, `statuses_attributes`).
pub(crate) async fn export_filters(pool: &PgPool, account_id: i64) -> Result<Vec<u8>> {
    let filters = sqlx::query(
        "SELECT id, title, context, action, expires_at FROM filters \
         WHERE account_id = $1 ORDER BY id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(filters.len());
    for f in &filters {
        let keywords = sqlx::query_as::<_, (String, bool)>(
            "SELECT keyword, whole_word FROM filter_keywords WHERE filter_id = $1 ORDER BY id",
        )
        .bind(f.get::<i64, _>("id"))
        .fetch_all(pool)
        .await?;
        let statuses = sqlx::query_scalar::<_, String>(
            "SELECT s.uri FROM filter_statuses fs JOIN statuses s ON s.id = fs.status_id \
             WHERE fs.filter_id = $1 AND s.uri IS NOT NULL ORDER BY fs.id",
        )
        .bind(f.get::<i64, _>("id"))
        .fetch_all(pool)
        .await?;
        out.push(json!({
            "title": f.get::<String, _>("title"),
            "context": f.get::<Vec<String>, _>("context"),
            "filter_action": f.get::<String, _>("action"),
            "expires_at": f.get::<Option<DateTime<Utc>>, _>("expires_at").map(|t| t.to_rfc3339()),
            "keywords_attributes": keywords.iter().map(|(k, w)| json!({
                "keyword": k, "whole_word": w,
            })).collect::<Vec<_>>(),
            "statuses_attributes": statuses,
        }));
    }
    serde_json::to_vec_pretty(&Value::Array(out))
        .map_err(|e| Error::Internal(format!("filters export: {e}")))
}
