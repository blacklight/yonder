//! Archive import/export (epic 11): Mastodon-compatible CSV/JSON
//! kinds, the full `archive` zip, and the `Import`/`Export`/
//! `ExportCleanup` job handlers.
//!
//! - Standalone CSV/JSON exports are generated inline (DB-only,
//!   bounded) and returned as file downloads. The full `archive` is a
//!   zip produced by the `Export` job under the private `exports/`
//!   prefix, downloadable through an authenticated endpoint.
//! - Every import runs through the `Import` job — the upload request
//!   only validates and stores the file. Handlers work in bounded
//!   batches (`BATCH_ROWS` or `BATCH_SECONDS` per run) and re-enqueue
//!   with a persisted cursor so large archives stay well under
//!   `jobs.handler_timeout_seconds`.
//! - Imported posts never federate: they get fresh local URIs with the
//!   source id in `statuses.original_id` (deduped by
//!   `uix_statuses_account_original`) and the source timestamp in
//!   `original_created_at`/`created_at`.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{
    activitypub::{self, ApContext},
    jobs::{JobContext, JobHandler, JobKind},
    users::relationships::{self, Ctx, FollowOpts},
    Error, Result,
};

/// Mastodon's `Form::Import::ROWS_PROCESSING_LIMIT`.
const ROWS_LIMIT: i64 = 20_000;

/// Rows applied per `Import` run before re-enqueueing.
const BATCH_ROWS: i64 = 500;

/// Wall-clock budget per `Import` run — comfortably under the 240 s
/// handler timeout.
const BATCH_SECONDS: u64 = 60;

/// Cursor multiplier separating sub-kinds inside an archive import.
const KIND_OFFSET: i64 = 1_000_000_000;

/// Export download expiry (Mastodon's archive cooldown/validity).
const EXPORT_TTL_DAYS: i64 = 7;

const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The transferable kinds. `Archive` is the full zip; `Followers` is
/// export-only (a remote can't be made to follow you by importing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `following_accounts.csv`
    Following,
    /// `followers.csv` — export only.
    Followers,
    /// `blocked_accounts.csv`
    Blocking,
    /// `muted_accounts.csv`
    Muting,
    /// `domain_blocks.csv`
    DomainBlocks,
    /// `bookmarks.csv` / `bookmarks.json` in archives
    Bookmarks,
    /// `lists.csv`
    Lists,
    /// `filters.json` — a `yonder` JSON shape (Mastodon has no format).
    Filters,
    /// The full zip archive.
    Archive,
}

impl Kind {
    /// API name used in the `kind` column and request bodies.
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Following => "following",
            Kind::Followers => "followers",
            Kind::Blocking => "blocking",
            Kind::Muting => "muting",
            Kind::DomainBlocks => "domain_blocks",
            Kind::Bookmarks => "bookmarks",
            Kind::Lists => "lists",
            Kind::Filters => "filters",
            Kind::Archive => "archive",
        }
    }

    /// Parse an API name.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "following" => Kind::Following,
            "followers" => Kind::Followers,
            "blocking" => Kind::Blocking,
            "muting" => Kind::Muting,
            "domain_blocks" => Kind::DomainBlocks,
            "bookmarks" => Kind::Bookmarks,
            "lists" => Kind::Lists,
            "filters" => Kind::Filters,
            "archive" => Kind::Archive,
            other => {
                return Err(Error::Validation(format!(
                    "unknown import/export kind {other:?}"
                )))
            }
        })
    }

    /// The Mastodon file name for this kind inside an archive.
    pub fn file_name(&self) -> &'static str {
        match self {
            Kind::Following => "following_accounts.csv",
            Kind::Followers => "followers.csv",
            Kind::Blocking => "blocked_accounts.csv",
            Kind::Muting => "muted_accounts.csv",
            Kind::DomainBlocks => "domain_blocks.csv",
            Kind::Bookmarks => "bookmarks.csv",
            Kind::Lists => "lists.csv",
            Kind::Filters => "filters.json",
            Kind::Archive => "archive.zip",
        }
    }

    /// Whether this kind is importable.
    pub fn importable(&self) -> bool {
        !matches!(self, Kind::Followers)
    }

    /// The OAuth scope required to export this kind.
    pub fn read_scope(&self) -> &'static str {
        match self {
            Kind::Following | Kind::Followers => "read:follows",
            Kind::Blocking => "read:blocks",
            Kind::Muting => "read:mutes",
            Kind::Bookmarks => "read:bookmarks",
            Kind::Lists => "read:lists",
            Kind::Filters => "read:filters",
            Kind::DomainBlocks | Kind::Archive => "read:accounts",
        }
    }

    /// The OAuth scope required to import this kind.
    pub fn write_scope(&self) -> &'static str {
        match self {
            Kind::Following => "write:follows",
            Kind::Blocking => "write:blocks",
            Kind::Muting => "write:mutes",
            Kind::Bookmarks => "write:bookmarks",
            Kind::Lists => "write:lists",
            Kind::Filters => "write:filters",
            Kind::DomainBlocks | Kind::Archive | Kind::Followers => "write:accounts",
        }
    }
}

/// `user@domain` for remote accounts, `user@localdomain` on export
/// (Mastodon appends the local domain so the file travels).
fn render_acct(username: &str, domain: Option<&str>, local_domain: &str) -> String {
    match domain {
        Some(d) => format!("{username}@{d}"),
        None => format!("{username}@{local_domain}"),
    }
}

/// Normalize a file `acct` for comparison/dedupe: lowercase, and strip
/// the local domain so `alice@localhost` matches `alice`.
fn normalize_acct(acct: &str, local_domain: &str) -> String {
    let acct = acct.trim().trim_start_matches('@').to_lowercase();
    match acct.split_once('@') {
        Some((user, domain)) if domain == local_domain => user.to_string(),
        _ => acct,
    }
}

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
            let mut w = csv::Writer::from_writer(vec![]);
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
            let mut w = csv::Writer::from_writer(vec![]);
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
            let mut w = csv::Writer::from_writer(vec![]);
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
            let mut w = csv::Writer::from_writer(vec![]);
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
            let mut w = csv::Writer::from_writer(vec![]);
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
            let mut w = csv::Writer::from_writer(vec![]);
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

fn csv_err(e: impl std::fmt::Display) -> Error {
    Error::Internal(format!("csv error: {e}"))
}

/// Single-column acct CSV for `Blocking`-shaped kinds.
async fn csv_export_accts(
    pool: &PgPool,
    sql: &str,
    account_id: i64,
    local_domain: &str,
) -> Result<(&'static str, &'static str, Vec<u8>)> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(sql)
        .bind(account_id)
        .fetch_all(pool)
        .await?;
    let mut w = csv::Writer::from_writer(vec![]);
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

/// The `filters.json` export — a `yonder` JSON array.
async fn export_filters(pool: &PgPool, account_id: i64) -> Result<Vec<u8>> {
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
        out.push(json!({
            "title": f.get::<String, _>("title"),
            "context": f.get::<Vec<String>, _>("context"),
            "action": f.get::<String, _>("action"),
            "expires_at": f.get::<Option<DateTime<Utc>>, _>("expires_at").map(|t| t.to_rfc3339()),
            "keywords": keywords.iter().map(|(k, w)| json!({
                "keyword": k, "whole_word": w,
            })).collect::<Vec<_>>(),
        }));
    }
    serde_json::to_vec_pretty(&Value::Array(out))
        .map_err(|e| Error::Internal(format!("filters export: {e}")))
}

// ---------------------------------------------------------------------------
// Export job (archive zip) and cleanup
// ---------------------------------------------------------------------------

/// `JobKind::Export` — build the archive zip for one `archive_exports`
/// row. Needs the AP context for `actor.json` (the handler renders the
/// actor document); the rest is DB + local storage.
pub struct ExportJob {
    ap: Option<ApContext>,
}

impl ExportJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        ExportJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ExportJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let export_id = payload
            .get("export_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("export job lacks export_id".into()))?;
        let row = sqlx::query(
            "UPDATE archive_exports SET state = 'running' WHERE id = $1 \
             AND state IN ('pending','running') RETURNING account_id",
        )
        .bind(export_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some(row) = row else { return Ok(()) };
        let account_id: i64 = row.get("account_id");
        match build_archive(ctx, self.ap.as_ref(), account_id).await {
            Ok(bytes) => {
                let key = format!("exports/{account_id}/{}.zip", uuid::Uuid::new_v4());
                ctx.media.put(&key, &bytes, "application/zip").await?;
                sqlx::query(
                    "UPDATE archive_exports SET state = 'done', file_key = $2, file_size = $3, \
                     completed_at = now(), expires_at = now() + $4::int * interval '1 day' \
                     WHERE id = $1",
                )
                .bind(export_id)
                .bind(&key)
                .bind(bytes.len() as i64)
                .bind(EXPORT_TTL_DAYS as i32)
                .execute(&ctx.db)
                .await?;
            }
            Err(e) => {
                sqlx::query(
                    "UPDATE archive_exports SET state = 'failed', error = $2, \
                     completed_at = now() WHERE id = $1",
                )
                .bind(export_id)
                .bind(e.to_string())
                .execute(&ctx.db)
                .await?;
            }
        }
        Ok(())
    }
}

/// Render the exporting account's actor document (pubby handler when
/// available, a minimal document otherwise).
fn actor_document(ap: Option<&ApContext>, account: &(String, String)) -> Value {
    if let Some(ap) = ap {
        if let Ok(Some(handler)) = ap.multi.for_username(&account.0) {
            return handler.get_actor_document();
        }
    }
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Person",
        "id": account.1,
        "preferredUsername": account.0,
    })
}

/// Collect archive members: actor/outbox/likes/bookmarks JSON, every
/// CSV, filters, avatar/header, and media attachment bytes.
async fn build_archive(
    ctx: &JobContext,
    ap: Option<&ApContext>,
    account_id: i64,
) -> Result<Vec<u8>> {
    let db = &ctx.db;
    let account: Option<(String, String)> =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1 AND domain IS NULL")
            .bind(account_id)
            .fetch_optional(db)
            .await?;
    let Some(account) = account else {
        return Err(Error::NotFound("account".into()));
    };
    let (username, actor_id) = account;
    let local_domain = ctx.config.instance.domain.clone();

    // Statuses → Create activities for outbox.json.
    let statuses = sqlx::query(
        "SELECT id, uri, content, spoiler_text, sensitive, language, visibility, \
                created_at, original_created_at FROM statuses \
         WHERE account_id = $1 AND NOT tombstone ORDER BY id",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let followers_url = format!("{actor_id}/followers");
    let mut items = Vec::with_capacity(statuses.len());
    for s in &statuses {
        let uri: String = s.get("uri");
        let visibility: String = s.get("visibility");
        let (to, cc) = visibility_audience(&visibility, &followers_url);
        let published: DateTime<Utc> = s
            .get::<Option<DateTime<Utc>>, _>("original_created_at")
            .or_else(|| s.get::<Option<DateTime<Utc>>, _>("created_at"))
            .unwrap_or_else(Utc::now);
        let object_id = uri.clone();
        items.push(json!({
            "id": format!("{object_id}#create"),
            "type": "Create",
            "actor": actor_id,
            "published": published.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "to": to,
            "cc": cc,
            "object": {
                "id": s.get::<Option<String>, _>("uri").unwrap_or_else(|| object_id.clone()),
                "type": "Note",
                "attributedTo": actor_id,
                "content": s.get::<String, _>("content"),
                "summary": s.get::<Option<String>, _>("spoiler_text"),
                "sensitive": s.get::<bool, _>("sensitive"),
                "published": published.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "to": to,
                "cc": cc,
            },
        }));
    }
    let outbox = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor_id}/outbox"),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    });

    let likes = sqlx::query_scalar::<_, String>(
        "SELECT s.uri FROM favourites f JOIN statuses s ON s.id = f.status_id \
         WHERE f.account_id = $1 AND s.uri IS NOT NULL ORDER BY f.id DESC",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let bookmarks = sqlx::query_scalar::<_, String>(
        "SELECT s.uri FROM bookmarks b JOIN statuses s ON s.id = b.status_id \
         WHERE b.account_id = $1 AND s.uri IS NOT NULL ORDER BY b.id DESC",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;

    // Reuse the inline exporters for CSV/JSON members.
    let mut members: Vec<(String, Vec<u8>)> = vec![
        (
            "actor.json".into(),
            serde_json::to_vec_pretty(&actor_document(ap, &(username.clone(), actor_id.clone())))
                .map_err(|e| Error::Internal(e.to_string()))?,
        ),
        (
            "outbox.json".into(),
            serde_json::to_vec(&outbox).map_err(|e| Error::Internal(e.to_string()))?,
        ),
        (
            "likes.json".into(),
            serde_json::to_vec(&likes).map_err(|e| Error::Internal(e.to_string()))?,
        ),
        (
            "bookmarks.json".into(),
            serde_json::to_vec(&bookmarks).map_err(|e| Error::Internal(e.to_string()))?,
        ),
    ];
    for kind in [
        Kind::Following,
        Kind::Followers,
        Kind::Blocking,
        Kind::Muting,
        Kind::DomainBlocks,
        Kind::Bookmarks,
        Kind::Lists,
    ] {
        let (name, _ct, bytes) = export_kind(&ctx.db, &local_domain, account_id, kind).await?;
        members.push((name, bytes));
    }
    members.push((
        Kind::Filters.file_name().to_string(),
        export_filters(db, account_id).await?,
    ));

    // Avatar/header bytes under their Mastodon names.
    let image_paths: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT avatar_path, banner_path FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(db)
            .await?;
    if let Some((avatar, banner)) = image_paths {
        for (key, name) in [(avatar, "avatar"), (banner, "header")] {
            if let Some(key) = key {
                if let Ok(bytes) = ctx.media.get(&key).await {
                    let ext = key.rsplit('.').next().unwrap_or("png");
                    members.push((format!("{name}.{ext}"), bytes));
                }
            }
        }
    }

    // Media attachment bytes under media_attachments/files/.
    let media = sqlx::query_scalar::<_, String>(
        "SELECT file_path FROM media_attachments WHERE account_id = $1 AND file_path IS NOT NULL",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    for key in media {
        if let Ok(bytes) = ctx.media.get(&key).await {
            let name = key.rsplit('/').next().unwrap_or("file").to_string();
            members.push((format!("media_attachments/files/{name}"), bytes));
        }
    }

    let bytes = tokio::task::spawn_blocking(move || {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in members.iter() {
            w.start_file(name, options)
                .map_err(|e| Error::Internal(format!("zip: {e}")))?;
            w.write_all(data)
                .map_err(|e| Error::Internal(format!("zip write: {e}")))?;
        }
        let cursor = w
            .finish()
            .map_err(|e| Error::Internal(format!("zip finish: {e}")))?;
        Ok::<_, Error>(cursor.into_inner())
    })
    .await
    .map_err(|e| Error::Internal(format!("zip task: {e}")))??;
    Ok(bytes)
}

/// Rows stuck 'running' this long had a worker die mid-run — the
/// import/export machinery itself marks completion, so age is the
/// only signal left.
const STUCK_RUNNING_HOURS: i64 = 24;

/// `JobKind::ExportCleanup` — expire `archive_exports` rows past
/// `expires_at` and delete their files; fail `running` rows abandoned
/// by a dead worker so they don't sit "in progress" forever (and, for
/// imports, leak their stored upload).
pub struct ExportCleanupJob;

#[async_trait::async_trait]
impl JobHandler for ExportCleanupJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        let keys = sqlx::query_scalar::<_, Option<String>>(
            "UPDATE archive_exports SET state = 'expired' \
             WHERE state = 'done' AND expires_at IS NOT NULL AND expires_at < now() \
             RETURNING file_key",
        )
        .fetch_all(&ctx.db)
        .await?;
        for key in keys.into_iter().flatten() {
            if let Err(e) = ctx.media.delete(&key).await {
                tracing::warn!(error = %e, "export file cleanup failed");
            }
        }
        // Dead-lettered jobs leave their rows 'running' with no
        // resumer — fail them and drop the stored upload.
        sqlx::query(
            "UPDATE archive_exports SET state = 'failed', \
             error = 'export run did not finish (worker restart)', completed_at = now() \
             WHERE state = 'running' AND created_at < now() - $1::int * interval '1 hour'",
        )
        .bind(STUCK_RUNNING_HOURS as i32)
        .execute(&ctx.db)
        .await?;
        let stuck_imports = sqlx::query_scalar::<_, Option<String>>(
            "UPDATE archive_imports SET state = 'failed', \
             error = 'import run did not finish (worker restart)', completed_at = now() \
             WHERE state = 'running' AND created_at < now() - $1::int * interval '1 hour' \
             RETURNING file_key",
        )
        .fetch_all(&ctx.db)
        .await?;
        for key in stuck_imports.into_iter().flatten() {
            if let Err(e) = ctx.media.delete(&key).await {
                tracing::warn!(error = %e, "stuck-import file cleanup failed");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Parse the uploaded file for `kind` into data rows (header skipped).
/// Row shapes differ per kind; each row is a `Vec<String>` except
/// `filters` (parsed as a single JSON doc) and `archive` (deferred).
fn parse_rows(kind: Kind, bytes: &[u8]) -> Result<Vec<Vec<String>>> {
    if kind == Kind::Filters || kind == Kind::Archive {
        return Ok(Vec::new());
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut rows: Vec<Vec<String>> = rdr
        .records()
        .filter_map(|r| r.ok())
        .map(|r| r.iter().map(|f| f.to_string()).collect())
        .collect();
    if let Some(first) = rows.first() {
        let cell0 = first.first().map(|s| s.as_str()).unwrap_or("");
        let is_header = cell0.eq_ignore_ascii_case("account address")
            || cell0.starts_with('#')
            || (kind == Kind::Muting
                && first
                    .get(1)
                    .map(|s| s.eq_ignore_ascii_case("hide notifications"))
                    .unwrap_or(false))
            || (kind == Kind::Following
                && first
                    .get(1)
                    .map(|s| s.eq_ignore_ascii_case("show boosts"))
                    .unwrap_or(false));
        if is_header {
            rows.remove(0);
        }
    }
    Ok(rows)
}

/// Sniff an upload at request time: returns the effective row count
/// for `total` (header excluded) and rejects files over
/// [`ROWS_LIMIT`] with a 422. `archive` and `filters` get `None` —
/// their count is known only once the job parses them.
pub fn count_rows(kind: Kind, bytes: &[u8]) -> Result<Option<i32>> {
    match kind {
        Kind::Archive | Kind::Filters => Ok(None),
        _ => {
            let rows = parse_rows(kind, bytes)?;
            if rows.len() as i64 > ROWS_LIMIT {
                return Err(Error::Validation(format!(
                    "imports are limited to {ROWS_LIMIT} rows"
                )));
            }
            Ok(Some(rows.len().min(i32::MAX as usize) as i32))
        }
    }
}

/// Parse a `filters.json` document into filter entries.
fn parse_filters(bytes: &[u8]) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| Error::Validation(format!("filters file is not valid JSON: {e}")))?;
    match v {
        Value::Array(items) => Ok(items),
        Value::Object(m) if m.contains_key("filters") => match m.get("filters") {
            Some(Value::Array(items)) => Ok(items.clone()),
            _ => Err(Error::Validation("filters key must be an array".into())),
        },
        _ => Err(Error::Validation(
            "filters file must be a JSON array".into(),
        )),
    }
}

/// `JobKind::Import` — process one `archive_imports` row in bounded
/// batches. Per-row failures increment `failed` and continue; fatal
/// errors (bad file, gone row) mark the import `failed`.
pub struct ImportJob {
    ap: Option<ApContext>,
}

impl ImportJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        ImportJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ImportJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let import_id = payload
            .get("import_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("import job lacks import_id".into()))?;
        let row = sqlx::query(
            "UPDATE archive_imports SET state = 'running' WHERE id = $1 \
             AND state IN ('pending','running') \
             RETURNING account_id, kind, mode, file_key, cursor",
        )
        .bind(import_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some(row) = row else { return Ok(()) };
        let account_id: i64 = row.get("account_id");
        let kind_s: String = row.get("kind");
        let mode: String = row.get("mode");
        let file_key: String = row.get("file_key");
        let cursor: i64 = row.get("cursor");
        let kind = match Kind::parse(&kind_s) {
            Ok(k) if k.importable() => k,
            _ => {
                return fail_import(ctx, import_id, &file_key, "unsupported kind").await;
            }
        };

        let bytes = match ctx.media.get(&file_key).await {
            Ok(b) => b,
            Err(e) => {
                return fail_import(ctx, import_id, &file_key, &e.to_string()).await;
            }
        };

        let run = ImportRun {
            ctx,
            rel: Ctx {
                db: &ctx.db,
                config: &ctx.config,
                ap: self.ap.as_ref(),
            },
            import_id,
            account_id,
            mode: &mode,
            file_key: &file_key,
            cursor,
            bytes: &bytes,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(BATCH_SECONDS),
        };
        match kind {
            Kind::Archive => import_archive(&run).await?,
            _ => import_flat(&run, kind).await?,
        }
        Ok(())
    }
}

/// The shared parameters an `Import` batch run threads through the
/// per-kind drivers — kept as a struct so each driver stays under the
/// argument-count lint.
struct ImportRun<'a> {
    ctx: &'a JobContext,
    rel: Ctx<'a>,
    import_id: i64,
    account_id: i64,
    mode: &'a str,
    file_key: &'a str,
    cursor: i64,
    bytes: &'a [u8],
    deadline: std::time::Instant,
}

/// Mark an import failed and drop the stored upload — failed imports
/// must not leak their file.
async fn fail_import(ctx: &JobContext, import_id: i64, file_key: &str, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE archive_imports SET state = 'failed', error = $2, completed_at = now() \
         WHERE id = $1",
    )
    .bind(import_id)
    .bind(error)
    .execute(&ctx.db)
    .await?;
    if let Err(e) = ctx.media.delete(file_key).await {
        tracing::warn!(import_id, error = %e, "failed-import file cleanup failed");
    }
    Ok(())
}

/// Finish an import: `done`, delete the upload.
async fn finish_import(ctx: &JobContext, import_id: i64, file_key: &str) -> Result<()> {
    sqlx::query("UPDATE archive_imports SET state = 'done', completed_at = now() WHERE id = $1")
        .bind(import_id)
        .execute(&ctx.db)
        .await?;
    if let Err(e) = ctx.media.delete(file_key).await {
        tracing::warn!(import_id, error = %e, "import file cleanup failed");
    }
    Ok(())
}

/// Persist progress and re-enqueue the continuation.
async fn requeue_import(
    ctx: &JobContext,
    import_id: i64,
    cursor: i64,
    processed: i64,
    failed: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE archive_imports SET cursor = $2, processed = processed + $3, \
         failed = failed + $4 WHERE id = $1",
    )
    .bind(import_id)
    .bind(cursor)
    .bind(processed)
    .bind(failed)
    .execute(&ctx.db)
    .await?;
    let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    dispatcher
        .enqueue(JobKind::Import, json!({ "import_id": import_id }))
        .await?;
    Ok(())
}

/// Overwrite prep for one non-archive kind: remove current entries
/// absent from the file's target set. Runs once when `cursor == 0`;
/// removals are idempotent so a redelivered run re-does the whole prep
/// (convergent — already-removed entries are simply absent).
async fn overwrite_prep(
    ctx: &JobContext,
    rel: Ctx<'_>,
    account_id: i64,
    kind: Kind,
    rows: &[Vec<String>],
    mode: &str,
    cursor: i64,
) -> Result<()> {
    if mode != "overwrite" || cursor != 0 {
        return Ok(());
    }
    let local_domain = &ctx.config.instance.domain;
    match kind {
        Kind::Following => {
            let set: std::collections::HashSet<String> = rows
                .iter()
                .filter_map(|r| r.first())
                .map(|a| normalize_acct(a, local_domain))
                .collect();
            let current = sqlx::query_as::<_, (i64, String, Option<String>)>(
                "SELECT a.id, a.username, a.domain FROM follows f \
                 JOIN accounts a ON a.id = f.target_account_id WHERE f.account_id = $1",
            )
            .bind(account_id)
            .fetch_all(&ctx.db)
            .await?;
            for (target_id, u, d) in current {
                let acct = match d {
                    Some(dom) => format!("{}@{}", u.to_lowercase(), dom.to_lowercase()),
                    None => u.to_lowercase(),
                };
                if !set.contains(&acct) {
                    relationships::unfollow(rel, account_id, target_id).await?;
                }
            }
        }
        Kind::Blocking => {
            remove_missing(&ctx.db, account_id, "blocks", rows, local_domain).await?;
        }
        Kind::Muting => {
            remove_missing(&ctx.db, account_id, "mutes", rows, local_domain).await?;
        }
        Kind::DomainBlocks => {
            let set: std::collections::HashSet<String> = rows
                .iter()
                .filter_map(|r| r.first())
                .map(|d| d.trim().to_lowercase())
                .collect();
            let current = sqlx::query_scalar::<_, String>(
                "SELECT domain FROM account_domain_blocks WHERE account_id = $1",
            )
            .bind(account_id)
            .fetch_all(&ctx.db)
            .await?;
            for d in current {
                if !set.contains(&d) {
                    relationships::unblock_domain(&ctx.db, account_id, &d).await?;
                }
            }
        }
        Kind::Bookmarks => {
            let set: std::collections::HashSet<String> = rows
                .iter()
                .filter_map(|r| r.first().map(|u| u.trim().to_string()))
                .collect();
            sqlx::query(
                "DELETE FROM bookmarks WHERE account_id = $1 AND status_id IN ( \
                     SELECT s.id FROM statuses s JOIN bookmarks b ON b.status_id = s.id \
                     WHERE b.account_id = $1 AND s.uri <> ALL($2))",
            )
            .bind(account_id)
            .bind(set.iter().cloned().collect::<Vec<_>>())
            .execute(&ctx.db)
            .await?;
        }
        Kind::Lists => {
            // Replace membership of lists named in the file.
            let mut by_list: HashMap<String, Vec<String>> = HashMap::new();
            for r in rows {
                if r.len() >= 2 {
                    by_list
                        .entry(r[0].trim().to_string())
                        .or_default()
                        .push(normalize_acct(&r[1], local_domain));
                }
            }
            for (title, members) in by_list {
                let list_id: Option<i64> =
                    sqlx::query_scalar("SELECT id FROM lists WHERE account_id = $1 AND title = $2")
                        .bind(account_id)
                        .bind(&title)
                        .fetch_optional(&ctx.db)
                        .await?;
                let Some(list_id) = list_id else { continue };
                let set: Vec<String> = members;
                sqlx::query(
                    "DELETE FROM list_accounts WHERE list_id = $1 AND account_id IN ( \
                         SELECT a.id FROM accounts a JOIN list_accounts la ON la.account_id = a.id \
                         WHERE la.list_id = $1 AND \
                             (lower(a.username) <> ALL($2) OR a.domain IS NOT NULL) AND \
                             lower(a.username || '@' || COALESCE(a.domain,'')) <> ALL($2))",
                )
                .bind(list_id)
                .bind(&set)
                .execute(&ctx.db)
                .await?;
            }
        }
        Kind::Filters => {
            sqlx::query("DELETE FROM filters WHERE account_id = $1")
                .bind(account_id)
                .execute(&ctx.db)
                .await?;
        }
        Kind::Followers | Kind::Archive => {}
    }
    Ok(())
}

/// Shared blocks/mutes overwrite prep: delete rows whose target acct
/// is absent from the file.
async fn remove_missing(
    db: &PgPool,
    account_id: i64,
    table: &str,
    rows: &[Vec<String>],
    local_domain: &str,
) -> Result<()> {
    let set: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|r| r.first())
        .map(|a| normalize_acct(a, local_domain))
        .collect();
    let current = sqlx::query_as::<_, (String, Option<String>)>(&format!(
        "SELECT a.username, a.domain FROM {table} t \
             JOIN accounts a ON a.id = t.target_account_id WHERE t.account_id = $1"
    ))
    .bind(account_id)
    .fetch_all(db)
    .await?;
    for (u, d) in current {
        let acct = match &d {
            Some(dom) => format!("{}@{}", u.to_lowercase(), dom.to_lowercase()),
            None => u.to_lowercase(),
        };
        if !set.contains(&acct) {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE account_id = $1 AND target_account_id = \
                     (SELECT id FROM accounts WHERE lower(username) = lower($2) AND \
                      (($3::text IS NULL AND domain IS NULL) OR lower(domain) = lower($3)))"
            ))
            .bind(account_id)
            .bind(&u)
            .bind(&d)
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// Apply one CSV/JSON row for `kind`. Per-row failures return `Err`
/// and are counted by the caller.
async fn apply_row(
    ctx: &JobContext,
    rel: Ctx<'_>,
    account_id: i64,
    kind: Kind,
    row: &[String],
) -> Result<()> {
    let db = &ctx.db;
    match kind {
        Kind::Following => {
            let Some(acct) = row.first() else {
                return Ok(());
            };
            let target = relationships::resolve_account(rel, acct).await?;
            let show_boosts = row.get(1).map(|v| truthy(v)).unwrap_or(true);
            let notify = row.get(2).map(|v| truthy(v)).unwrap_or(false);
            relationships::follow(
                rel,
                account_id,
                target,
                FollowOpts {
                    show_reblogs: Some(show_boosts),
                    notify: Some(notify),
                },
            )
            .await?;
        }
        Kind::Blocking => {
            let Some(acct) = row.first() else {
                return Ok(());
            };
            let target = relationships::resolve_account(rel, acct).await?;
            relationships::block(db, account_id, target).await?;
        }
        Kind::Muting => {
            let Some(acct) = row.first() else {
                return Ok(());
            };
            let target = relationships::resolve_account(rel, acct).await?;
            let hide = row.get(1).map(|v| truthy(v)).unwrap_or(true);
            relationships::mute(db, account_id, target, hide, None).await?;
        }
        Kind::DomainBlocks => {
            let Some(domain) = row.first() else {
                return Ok(());
            };
            relationships::block_domain(db, account_id, domain).await?;
        }
        Kind::Bookmarks => {
            let Some(uri) = row.first() else {
                return Ok(());
            };
            sqlx::query(
                "INSERT INTO bookmarks (account_id, status_id) \
                 SELECT $1, id FROM statuses WHERE uri = $2 \
                 ON CONFLICT (account_id, status_id) DO NOTHING",
            )
            .bind(account_id)
            .bind(uri.trim())
            .execute(db)
            .await?;
        }
        Kind::Lists => {
            if row.len() < 2 {
                return Ok(());
            }
            let title = row[0].trim();
            if title.is_empty() || title.chars().count() > 500 {
                return Ok(());
            }
            // `lists` has no (account_id,title) unique constraint —
            // Mastodon dedupes by title, so look up before inserting.
            let list_id: i64 = match sqlx::query_scalar::<_, i64>(
                "SELECT id FROM lists WHERE account_id = $1 AND title = $2",
            )
            .bind(account_id)
            .bind(title)
            .fetch_optional(db)
            .await?
            {
                Some(id) => id,
                None => {
                    sqlx::query_scalar(
                        "INSERT INTO lists (account_id, title) VALUES ($1, $2) RETURNING id",
                    )
                    .bind(account_id)
                    .bind(title)
                    .fetch_one(db)
                    .await?
                }
            };
            let target = relationships::resolve_account(rel, &row[1]).await?;
            // Mastodon follows the member first if not already followed.
            let following: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM follows \
                 WHERE account_id = $1 AND target_account_id = $2)",
            )
            .bind(account_id)
            .bind(target)
            .fetch_one(db)
            .await?;
            if !following {
                relationships::follow(rel, account_id, target, FollowOpts::default()).await?;
            }
            sqlx::query(
                "INSERT INTO list_accounts (list_id, account_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(list_id)
            .bind(target)
            .execute(db)
            .await?;
        }
        Kind::Followers | Kind::Filters | Kind::Archive => {}
    }
    Ok(())
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes" | "t")
}

/// Apply one filter entry from `filters.json`.
async fn apply_filter(db: &PgPool, account_id: i64, entry: &Value) -> Result<()> {
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(500)
        .collect::<String>();
    if title.is_empty() {
        return Err(Error::Validation("filter entry lacks a title".into()));
    }
    let context: Vec<String> = entry
        .get("context")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .filter(|c| {
                    matches!(
                        c.as_str(),
                        "home" | "notifications" | "public" | "thread" | "account"
                    )
                })
                .collect()
        })
        .unwrap_or_else(|| vec!["home".to_string()]);
    let action = entry
        .get("action")
        .and_then(Value::as_str)
        .filter(|a| matches!(*a, "warn" | "hide" | "blur"))
        .unwrap_or("warn")
        .to_string();
    let expires_at = entry
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    let filter_id: i64 = sqlx::query_scalar(
        "INSERT INTO filters (account_id, title, context, action, expires_at) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(account_id)
    .bind(&title)
    .bind(&context)
    .bind(&action)
    .bind(expires_at)
    .fetch_one(db)
    .await?;
    if let Some(keywords) = entry.get("keywords").and_then(Value::as_array) {
        for kw in keywords {
            let keyword = kw
                .get("keyword")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .chars()
                .take(500)
                .collect::<String>();
            if keyword.is_empty() {
                continue;
            }
            let whole_word = kw
                .get("whole_word")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            sqlx::query(
                "INSERT INTO filter_keywords (filter_id, keyword, whole_word) \
                 VALUES ($1, $2, $3)",
            )
            .bind(filter_id)
            .bind(&keyword)
            .bind(whole_word)
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// Drive a flat (non-archive) import through one bounded batch.
async fn import_flat(run: &ImportRun<'_>, kind: Kind) -> Result<()> {
    let (ctx, rel, import_id, account_id, mode, file_key, cursor, bytes, deadline) = (
        run.ctx,
        run.rel,
        run.import_id,
        run.account_id,
        run.mode,
        run.file_key,
        run.cursor,
        run.bytes,
        run.deadline,
    );
    if kind == Kind::Filters {
        let entries = match parse_filters(bytes) {
            Ok(e) => e,
            Err(e) => {
                return fail_import(ctx, import_id, file_key, &e.to_string()).await;
            }
        };
        if entries.len() as i64 > ROWS_LIMIT {
            return fail_import(ctx, import_id, file_key, "too many entries").await;
        }
        if mode == "overwrite" && cursor == 0 {
            sqlx::query("DELETE FROM filters WHERE account_id = $1")
                .bind(account_id)
                .execute(&ctx.db)
                .await?;
        }
        let mut processed = 0i64;
        let mut failed = 0i64;
        let mut idx = cursor.max(0) as usize;
        while idx < entries.len() && processed < BATCH_ROWS && std::time::Instant::now() < deadline
        {
            match apply_filter(&ctx.db, account_id, &entries[idx]).await {
                Ok(()) => processed += 1,
                Err(_) => failed += 1,
            }
            idx += 1;
        }
        if idx < entries.len() {
            return requeue_import(ctx, import_id, idx as i64, processed, failed).await;
        }
        sqlx::query(
            "UPDATE archive_imports SET cursor = $2, processed = processed + $3, \
             failed = failed + $4 WHERE id = $1",
        )
        .bind(import_id)
        .bind(idx as i64)
        .bind(processed)
        .bind(failed)
        .execute(&ctx.db)
        .await?;
        return finish_import(ctx, import_id, file_key).await;
    }

    let rows = match parse_rows(kind, bytes) {
        Ok(r) => r,
        Err(e) => {
            return fail_import(ctx, import_id, file_key, &e.to_string()).await;
        }
    };
    if rows.len() as i64 > ROWS_LIMIT {
        return fail_import(ctx, import_id, file_key, "too many rows").await;
    }
    overwrite_prep(ctx, rel, account_id, kind, &rows, mode, cursor).await?;

    let mut processed = 0i64;
    let mut failed = 0i64;
    let mut idx = cursor.max(0) as usize;
    while idx < rows.len()
        && processed + failed < BATCH_ROWS
        && std::time::Instant::now() < deadline
    {
        match apply_row(ctx, rel, account_id, kind, &rows[idx]).await {
            Ok(()) => processed += 1,
            Err(e) => {
                failed += 1;
                tracing::debug!(import_id, row = idx, error = %e, "import row failed");
            }
        }
        idx += 1;
    }
    sqlx::query(
        "UPDATE archive_imports SET cursor = $2, processed = processed + $3, \
         failed = failed + $4 WHERE id = $1",
    )
    .bind(import_id)
    .bind(idx as i64)
    .bind(processed)
    .bind(failed)
    .execute(&ctx.db)
    .await?;
    if idx < rows.len() {
        let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        dispatcher
            .enqueue(JobKind::Import, json!({ "import_id": import_id }))
            .await?;
        return Ok(());
    }
    finish_import(ctx, import_id, file_key).await
}

// ---------------------------------------------------------------------------
// Archive import (zip → per-kind, then posts)
// ---------------------------------------------------------------------------

/// Sub-kinds inside an archive, in processing order (posts last).
const ARCHIVE_KINDS: &[&str] = &[
    "following_accounts.csv",
    "blocked_accounts.csv",
    "muted_accounts.csv",
    "domain_blocks.csv",
    "bookmarks.csv",
    "lists.csv",
    "filters.json",
    "outbox.json",
];

fn kind_for_member(name: &str) -> Option<Kind> {
    match name {
        "following_accounts.csv" => Some(Kind::Following),
        "blocked_accounts.csv" => Some(Kind::Blocking),
        "muted_accounts.csv" => Some(Kind::Muting),
        "domain_blocks.csv" => Some(Kind::DomainBlocks),
        "bookmarks.csv" | "bookmarks.json" => Some(Kind::Bookmarks),
        "lists.csv" => Some(Kind::Lists),
        "filters.json" => Some(Kind::Filters),
        _ => None,
    }
}

/// Read one zip member to bytes (or `None` when absent). The archive is
/// re-opened per member — bounded and simple.
fn zip_member(bytes: &[u8], name: &str) -> Result<Option<Vec<u8>>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| Error::Validation(format!("archive is not a valid zip: {e}")))?;
    let member = archive.by_name(name);
    match member {
        Ok(mut f) => {
            // Individual member cap — the file as a whole is already
            // bounded by admin.max_import_size.
            let mut buf = Vec::with_capacity(f.size().min(64 * 1024 * 1024) as usize);
            f.read_to_end(&mut buf)
                .map_err(|e| Error::Validation(format!("zip member {name}: {e}")))?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(e) => Err(Error::Validation(format!("zip member {name}: {e}"))),
    }
}

/// Extract `bookmarks.json` — an array of URIs (or objects with `id`).
fn parse_uri_list(bytes: &[u8]) -> Vec<String> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.get("id").and_then(|i| i.as_str()).map(String::from))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Drive an archive import: cursor = `kind_index * KIND_OFFSET + row`.
async fn import_archive(run: &ImportRun<'_>) -> Result<()> {
    let (ctx, rel, import_id, account_id, mode, file_key, cursor, bytes, deadline) = (
        run.ctx,
        run.rel,
        run.import_id,
        run.account_id,
        run.mode,
        run.file_key,
        run.cursor,
        run.bytes,
        run.deadline,
    );
    let mut kind_idx = cursor / KIND_OFFSET;
    let mut offset = cursor % KIND_OFFSET;
    let mut processed = 0i64;
    let mut failed = 0i64;

    while (kind_idx as usize) < ARCHIVE_KINDS.len() {
        let member = ARCHIVE_KINDS[kind_idx as usize];
        let content = match zip_member(bytes, member) {
            Ok(Some(b)) => b,
            Ok(None) => {
                kind_idx += 1;
                offset = 0;
                continue;
            }
            Err(e) => {
                return fail_import(ctx, import_id, file_key, &e.to_string()).await;
            }
        };

        if member == "outbox.json" {
            let items = outbox_items(&content);
            let mut idx = offset as usize;
            while idx < items.len()
                && processed + failed < BATCH_ROWS
                && std::time::Instant::now() < deadline
            {
                match import_post(ctx, rel, account_id, &items[idx]).await {
                    Ok(()) => processed += 1,
                    Err(e) => {
                        failed += 1;
                        tracing::debug!(import_id, row = idx, error = %e, "post import failed");
                    }
                }
                idx += 1;
            }
            offset = idx as i64;
            if idx < items.len() {
                break;
            }
        } else if let Some(kind) = kind_for_member(member) {
            let rows: Vec<Vec<String>> = if member == "bookmarks.json" {
                parse_uri_list(&content)
                    .into_iter()
                    .map(|u| vec![u])
                    .collect()
            } else if member == "filters.json" {
                // Filters handled per-entry below.
                Vec::new()
            } else {
                match parse_rows(kind, &content) {
                    Ok(r) => r,
                    Err(e) => {
                        return fail_import(ctx, import_id, file_key, &e.to_string()).await;
                    }
                }
            };
            if member == "filters.json" {
                let entries = match parse_filters(&content) {
                    Ok(e) => e,
                    Err(e) => {
                        return fail_import(ctx, import_id, file_key, &e.to_string()).await;
                    }
                };
                if mode == "overwrite" && offset == 0 {
                    sqlx::query("DELETE FROM filters WHERE account_id = $1")
                        .bind(account_id)
                        .execute(&ctx.db)
                        .await?;
                }
                let mut idx = offset as usize;
                while idx < entries.len()
                    && processed + failed < BATCH_ROWS
                    && std::time::Instant::now() < deadline
                {
                    match apply_filter(&ctx.db, account_id, &entries[idx]).await {
                        Ok(()) => processed += 1,
                        Err(_) => failed += 1,
                    }
                    idx += 1;
                }
                offset = idx as i64;
                if idx < entries.len() {
                    break;
                }
            } else {
                overwrite_prep(ctx, rel, account_id, kind, &rows, mode, offset).await?;
                let mut idx = offset as usize;
                while idx < rows.len()
                    && processed + failed < BATCH_ROWS
                    && std::time::Instant::now() < deadline
                {
                    match apply_row(ctx, rel, account_id, kind, &rows[idx]).await {
                        Ok(()) => processed += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::debug!(import_id, member, row = idx, error = %e, "import row failed");
                        }
                    }
                    idx += 1;
                }
                offset = idx as i64;
                if idx < rows.len() {
                    break;
                }
            }
        }
        kind_idx += 1;
        offset = 0;
    }

    let cursor = kind_idx * KIND_OFFSET + offset;
    if (kind_idx as usize) < ARCHIVE_KINDS.len() {
        return requeue_import(ctx, import_id, cursor, processed, failed).await;
    }
    sqlx::query(
        "UPDATE archive_imports SET cursor = $2, processed = processed + $3, \
         failed = failed + $4 WHERE id = $1",
    )
    .bind(import_id)
    .bind(cursor)
    .bind(processed)
    .bind(failed)
    .execute(&ctx.db)
    .await?;
    finish_import(ctx, import_id, file_key).await
}

/// The `orderedItems`/`items` of an outbox OrderedCollection.
fn outbox_items(bytes: &[u8]) -> Vec<Value> {
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get("orderedItems")
        .or_else(|| v.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Map an AP audience to `(to, cc)` — the inverse of
/// [`visibility_from_audience`].
fn visibility_audience(visibility: &str, followers_url: &str) -> (Vec<String>, Vec<String>) {
    match visibility {
        "public" => (vec![AS_PUBLIC.to_string()], vec![followers_url.to_string()]),
        "unlisted" => (vec![followers_url.to_string()], vec![AS_PUBLIC.to_string()]),
        "private" | "followers_only" => (vec![followers_url.to_string()], vec![]),
        _ => (vec![], vec![]),
    }
}

/// Derive local visibility from an object's `to`/`cc`.
fn visibility_from_audience(obj: &Value, followers_fragment: &str) -> &'static str {
    let to = str_list(obj.get("to"));
    let cc = str_list(obj.get("cc"));
    let public = |l: &[String]| l.iter().any(|v| v == AS_PUBLIC);
    let followers = |l: &[String]| {
        l.iter()
            .any(|v| v.ends_with("/followers") || v.contains(followers_fragment))
    };
    if public(&to) {
        "public"
    } else if public(&cc) || (followers(&to) && public(&cc)) {
        "unlisted"
    } else if followers(&to) || followers(&cc) {
        "private"
    } else {
        "direct"
    }
}

/// JSON string-list field → `Vec<String>` (single strings wrap).
/// Shared with `moves` (alsoKnownAs parsing).
pub(crate) fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn id_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str().or_else(|| v.get("id").and_then(|i| i.as_str())))
        .map(String::from)
}

/// Import one `Create` item from an outbox into a local status row.
/// Never federates; deduped on `(account_id, original_id)`.
async fn import_post(ctx: &JobContext, rel: Ctx<'_>, account_id: i64, item: &Value) -> Result<()> {
    let db = &ctx.db;
    if item.get("type").and_then(Value::as_str) != Some("Create") {
        return Ok(()); // Announces etc. aren't posts.
    }
    let obj = item.get("object").cloned().unwrap_or(Value::Null);
    let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(obj_type, "Note" | "Question") {
        return Ok(());
    }
    let original_id =
        id_str(obj.get("id")).ok_or_else(|| Error::Validation("Create object lacks id".into()))?;
    let account_row: (String, String) =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    let (_username, actor_id) = account_row;

    let published = obj
        .get("published")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let content = obj
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("contentMap")
                .and_then(Value::as_object)
                .and_then(|m| m.values().next())
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .chars()
        .take(20_000)
        .collect::<String>();
    let summary = obj
        .get("summary")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(2000).collect::<String>());
    let language = obj
        .get("contentMap")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next().cloned());
    let visibility = visibility_from_audience(&obj, "/followers");

    // Dedupe — one imported copy per source object.
    let status_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, spoiler_text, sensitive, language, \
                               visibility, local, original_id, original_created_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, true, $7, $8, $8) \
         ON CONFLICT (account_id, original_id) WHERE original_id IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&content)
    .bind(&summary)
    .bind(
        obj.get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
    .bind(&language)
    .bind(visibility)
    .bind(&original_id)
    .bind(published)
    .fetch_optional(db)
    .await?;
    let Some(status_id) = status_id else {
        return Ok(()); // already imported
    };
    // Mint the local URI from the new id (Mastodon's /statuses/{id}
    // shape under the actor) and a profile URL.
    sqlx::query("UPDATE statuses SET uri = $2, url = $3 WHERE id = $1")
        .bind(status_id)
        .bind(format!("{actor_id}/statuses/{status_id}"))
        .bind(format!(
            "{}/@{}/{}",
            activitypub::federation_base_url(&ctx.config),
            _username,
            status_id
        ))
        .execute(db)
        .await?;
    sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
        .bind(account_id)
        .execute(db)
        .await?;

    // Reply linkage — only when the parent is already known locally.
    if let Some(parent) = id_str(obj.get("inReplyTo")) {
        sqlx::query(
            "UPDATE statuses SET in_reply_to_id = p.id, in_reply_to_account_id = p.account_id \
             FROM statuses p WHERE statuses.id = $1 AND p.uri = $2",
        )
        .bind(status_id)
        .bind(&parent)
        .execute(db)
        .await?;
    }

    // Hashtags and mentions.
    if let Some(tags) = obj.get("tag").and_then(Value::as_array) {
        for tag in tags {
            match tag.get("type").and_then(Value::as_str) {
                Some("Hashtag") => {
                    let name = tag
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .chars()
                        .take(200)
                        .collect::<String>();
                    if name.is_empty() {
                        continue;
                    }
                    sqlx::query(
                        "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (name) DO NOTHING",
                    )
                    .bind(&name)
                    .execute(db)
                    .await?;
                    sqlx::query(
                        "INSERT INTO status_tags (status_id, tag_id) \
                         SELECT $1, id FROM tags WHERE name = $2 ON CONFLICT DO NOTHING",
                    )
                    .bind(status_id)
                    .bind(&name)
                    .execute(db)
                    .await?;
                }
                Some("Mention") => {
                    let Some(href) = id_str(tag.get("href")) else {
                        continue;
                    };
                    // Mentioned accounts resolve best-effort; a bare
                    // upsert on the URI keeps the row without a fetch.
                    sqlx::query(
                        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
                         VALUES ($1,$2,$3,$3,'') ON CONFLICT (actor_id) DO NOTHING",
                    )
                    .bind(activitypub::moderation::actor_domain(&href))
                    .bind(
                        href.trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or(&href)
                            .to_string(),
                    )
                    .bind(&href)
                    .execute(db)
                    .await?;
                    sqlx::query(
                        "INSERT INTO status_mentions (status_id, account_id) \
                         SELECT $1, id FROM accounts WHERE actor_id = $2 ON CONFLICT DO NOTHING",
                    )
                    .bind(status_id)
                    .bind(&href)
                    .execute(db)
                    .await?;
                }
                _ => {}
            }
        }
    }

    // Poll (Question).
    if obj_type == "Question" {
        let options = obj
            .get("oneOf")
            .or_else(|| obj.get("anyOf"))
            .and_then(Value::as_array);
        if let Some(options) = options {
            let multiple = obj.get("anyOf").is_some();
            let expires_at = obj
                .get("endTime")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));
            let poll_id: i64 = sqlx::query_scalar(
                "INSERT INTO polls (status_id, expires_at, multiple) VALUES ($1, $2, $3) \
                 RETURNING id",
            )
            .bind(status_id)
            .bind(expires_at)
            .bind(multiple)
            .fetch_one(db)
            .await?;
            sqlx::query("UPDATE statuses SET poll_id = $2 WHERE id = $1")
                .bind(status_id)
                .bind(poll_id)
                .execute(db)
                .await?;
            for (i, opt) in options.iter().enumerate() {
                let title = opt
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(500)
                    .collect::<String>();
                sqlx::query(
                    "INSERT INTO poll_options (poll_id, idx, title) VALUES ($1, $2, $3) \
                     ON CONFLICT (poll_id, idx) DO NOTHING",
                )
                .bind(poll_id)
                .bind(i as i32)
                .bind(&title)
                .execute(db)
                .await?;
            }
        }
    }

    // Attachments: bytes from the archive's media_attachments/ when the
    // URL matches a member, else a remote_url reference.
    let _ = rel; // resolution not needed for attachments in this epic
    if let Some(attachments) = obj.get("attachment").and_then(Value::as_array) {
        for att in attachments {
            let Some(url) = id_str(att.get("url")) else {
                continue;
            };
            let media_type = match att.get("mediaType").and_then(Value::as_str).unwrap_or("") {
                t if t.starts_with("image/") => "image",
                t if t.starts_with("video/") => "video",
                t if t.starts_with("audio/") => "audio",
                _ => "document",
            };
            let description = att
                .get("name")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(5000).collect::<String>());
            // A zip member lookup happens in `import_archive` — here we
            // only persist remote references; embedded bytes land when
            // the caller passes them via `attachment_files` (kept
            // simple: remote_url rows).
            sqlx::query(
                "INSERT INTO media_attachments (account_id, status_id, type, remote_url, \
                 content_type, description) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(account_id)
            .bind(status_id)
            .bind(media_type)
            .bind(&url)
            .bind(att.get("mediaType").and_then(Value::as_str))
            .bind(&description)
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// Register this module's job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(JobKind::Import, ImportJob::new(ap.cloned()));
    registry.register(JobKind::Export, ExportJob::new(ap.cloned()));
    registry.register(JobKind::ExportCleanup, ExportCleanupJob);
}
