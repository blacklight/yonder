//! Import job orchestration (epic 31 section 7): the `Import` handler,
//! CSV/JSON row application, overwrite prep, and progress accounting.

use super::restore::{apply_collection_item, import_archive};
use super::*;
use crate::{
    activitypub::ApContext,
    jobs::{JobContext, JobHandler, JobKind},
    users::relationships::{self, Ctx, FollowOpts},
    Error, Result,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Parse the uploaded file for `kind` into data rows (header skipped).
/// Row shapes differ per kind; each row is a `Vec<String>` except
/// `filters` (parsed as a single JSON doc) and `archive` (deferred).
pub(crate) fn parse_rows(kind: Kind, bytes: &[u8]) -> Result<Vec<Vec<String>>> {
    if kind == Kind::Filters || kind == Kind::Archive {
        return Ok(Vec::new());
    }
    let mut rdr = ::csv::ReaderBuilder::new()
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
pub(crate) fn parse_filters(bytes: &[u8]) -> Result<Vec<Value>> {
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

        // A missing upload can never succeed on retry; any other read
        // error is transient storage trouble and returns to the job
        // framework so the upload survives for the retry.
        let bytes = match ctx.media.get(&file_key).await {
            Ok(b) => b,
            Err(e @ Error::NotFound(_)) => {
                return fail_import(ctx, import_id, &file_key, &e.to_string()).await;
            }
            Err(e) => return Err(e),
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
            bytes,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(BATCH_SECONDS),
        };
        match kind {
            Kind::Archive => import_archive(run).await?,
            _ => import_flat(run, kind).await?,
        }
        Ok(())
    }
}

/// The shared parameters an `Import` batch run threads through the
/// per-kind drivers — kept as a struct so each driver stays under the
/// argument-count lint.
pub(crate) struct ImportRun<'a> {
    pub(crate) ctx: &'a JobContext,
    pub(crate) rel: Ctx<'a>,
    pub(crate) import_id: i64,
    pub(crate) account_id: i64,
    pub(crate) mode: &'a str,
    pub(crate) file_key: &'a str,
    pub(crate) cursor: i64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) deadline: std::time::Instant,
}

/// Mark an import failed and drop the stored upload — failed imports
/// must not leak their file.
pub(crate) async fn fail_import(
    ctx: &JobContext,
    import_id: i64,
    file_key: &str,
    error: &str,
) -> Result<()> {
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
pub(crate) async fn finish_import(ctx: &JobContext, import_id: i64, file_key: &str) -> Result<()> {
    sqlx::query("UPDATE archive_imports SET state = 'done', completed_at = now() WHERE id = $1")
        .bind(import_id)
        .execute(&ctx.db)
        .await?;
    if let Err(e) = ctx.media.delete(file_key).await {
        tracing::warn!(import_id, error = %e, "import file cleanup failed");
    }
    Ok(())
}

/// Per-batch outcome accounting — inserted, already-present, skipped,
/// unsupported and failed are tracked independently (contract §7.6).
/// `processed`/`failed` columns carry applied/failed; the rest accrue
/// into the `summary` JSONB.
#[derive(Default)]
pub(crate) struct Tally {
    pub applied: i64,
    pub failed: i64,
    pub present: i64,
    pub skipped: i64,
    pub unsupported: i64,
    pub media_unavailable: i64,
}

impl Tally {
    /// Rows examined this run — the batch-budget denominator.
    pub fn seen(&self) -> i64 {
        self.applied + self.failed + self.present + self.skipped + self.unsupported
    }
}

/// Persist a batch's progress (cursor, counters, summary merge).
pub(crate) async fn persist_progress(
    ctx: &JobContext,
    import_id: i64,
    cursor: i64,
    tally: &Tally,
) -> Result<()> {
    sqlx::query(
        "UPDATE archive_imports SET cursor = $2, processed = processed + $3, \
         failed = failed + $4, \
         summary = jsonb_build_object( \
           'already_present', COALESCE((summary->>'already_present')::int, 0) + $5, \
           'skipped', COALESCE((summary->>'skipped')::int, 0) + $6, \
           'unsupported', COALESCE((summary->>'unsupported')::int, 0) + $7, \
           'media_unavailable', COALESCE((summary->>'media_unavailable')::int, 0) + $8) \
         WHERE id = $1",
    )
    .bind(import_id)
    .bind(cursor)
    .bind(tally.applied)
    .bind(tally.failed)
    .bind(tally.present)
    .bind(tally.skipped)
    .bind(tally.unsupported)
    .bind(tally.media_unavailable)
    .execute(&ctx.db)
    .await?;
    Ok(())
}

/// Persist progress and re-enqueue the continuation.
pub(crate) async fn requeue_import(
    ctx: &JobContext,
    import_id: i64,
    cursor: i64,
    tally: &Tally,
) -> Result<()> {
    persist_progress(ctx, import_id, cursor, tally).await?;
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
pub(crate) async fn overwrite_prep(
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
pub(crate) async fn remove_missing(
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
pub(crate) async fn apply_row(
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
            apply_collection_item(ctx, account_id, uri.trim(), false).await?;
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

pub(crate) fn truthy(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes" | "t")
}

/// Apply one filter entry from `filters.json`.
pub(crate) async fn apply_filter(db: &PgPool, account_id: i64, entry: &Value) -> Result<()> {
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
pub(crate) async fn import_flat(run: ImportRun<'_>, kind: Kind) -> Result<()> {
    let ImportRun {
        ctx,
        rel,
        import_id,
        account_id,
        mode,
        file_key,
        cursor,
        bytes,
        deadline,
    } = run;
    if kind == Kind::Filters {
        let entries = match parse_filters(&bytes) {
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
        let mut tally = Tally::default();
        let mut idx = cursor.max(0) as usize;
        while idx < entries.len()
            && tally.seen() < BATCH_ROWS
            && std::time::Instant::now() < deadline
        {
            match apply_filter(&ctx.db, account_id, &entries[idx]).await {
                Ok(()) => tally.applied += 1,
                Err(_) => tally.failed += 1,
            }
            idx += 1;
        }
        if idx < entries.len() {
            return requeue_import(ctx, import_id, idx as i64, &tally).await;
        }
        persist_progress(ctx, import_id, idx as i64, &tally).await?;
        return finish_import(ctx, import_id, file_key).await;
    }

    let rows = match parse_rows(kind, &bytes) {
        Ok(r) => r,
        Err(e) => {
            return fail_import(ctx, import_id, file_key, &e.to_string()).await;
        }
    };
    if rows.len() as i64 > ROWS_LIMIT {
        return fail_import(ctx, import_id, file_key, "too many rows").await;
    }
    overwrite_prep(ctx, rel, account_id, kind, &rows, mode, cursor).await?;

    let mut tally = Tally::default();
    let mut idx = cursor.max(0) as usize;
    while idx < rows.len() && tally.seen() < BATCH_ROWS && std::time::Instant::now() < deadline {
        match apply_row(ctx, rel, account_id, kind, &rows[idx]).await {
            Ok(()) => tally.applied += 1,
            Err(e) => {
                tally.failed += 1;
                tracing::debug!(import_id, row = idx, error = %e, "import row failed");
            }
        }
        idx += 1;
    }
    if idx < rows.len() {
        return requeue_import(ctx, import_id, idx as i64, &tally).await;
    }
    persist_progress(ctx, import_id, idx as i64, &tally).await?;
    finish_import(ctx, import_id, file_key).await
}
