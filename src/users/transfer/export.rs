//! Archive export (epic 31 section 7): the `Export` job builds the
//! Mastodon-shaped zip; `ExportCleanup` expires files and fails
//! abandoned runs.

use super::csv::*;
use super::restore::visibility_audience;
use super::*;
use crate::{
    activitypub::ApContext,
    jobs::{JobContext, JobHandler},
    Error, Result,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::Row;
use std::io::{Cursor, Write};

// ---------------------------------------------------------------------------
// Export job (archive zip) and cleanup
// ---------------------------------------------------------------------------

/// Archive member listing storage keys whose media object could not
/// be read — dangling outbox references are reported, never silent.
const MISSING_MEDIA_FILE: &str = "missing_media.txt";

/// The member name for one attachment's embedded bytes: `{att_id}`
/// keeps it unique even when several rows share a `file_path`
/// (content-addressed imports store identical bytes under one key) —
/// every outbox `url` must name a member that actually exists.
fn media_member_name(att_id: i64, key: &str) -> String {
    let base = key.rsplit('/').next().unwrap_or("file");
    format!("media_attachments/files/{att_id}-{base}")
}

/// ActivityStreams timestamps: second precision, `Z` suffix.
fn ap_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

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
                // Commit only while the row is still running and the
                // account is still eligible. A purge sweeps the object
                // prefix after deleting these rows, so a zero-row update
                // means the object we just wrote has no owner: remove it.
                let committed = sqlx::query(
                    "UPDATE archive_exports SET state = 'done', file_key = $2, file_size = $3, \
                     completed_at = now(), expires_at = now() + $4::int * interval '1 day' \
                     WHERE id = $1 AND state = 'running' \
                       AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = archive_exports.account_id \
                                       AND (a.deleted_at IS NOT NULL OR a.disabled_at IS NOT NULL)) \
                       AND NOT EXISTS (SELECT 1 FROM account_deletions d \
                                       WHERE d.account_id = archive_exports.account_id \
                                         AND d.state IN ('pending','running'))",
                )
                .bind(export_id)
                .bind(&key)
                .bind(bytes.len() as i64)
                .bind(EXPORT_TTL_DAYS as i32)
                .execute(&ctx.db)
                .await?
                .rows_affected();
                if committed == 0 {
                    if let Err(e) = ctx.media.delete(&key).await {
                        tracing::warn!(export_id, error = %e, "abandoned export file cleanup failed");
                    }
                    sqlx::query(
                        "UPDATE archive_exports SET state = 'failed', \
                         error = 'account is no longer eligible for export', completed_at = now() \
                         WHERE id = $1 AND state = 'running'",
                    )
                    .bind(export_id)
                    .execute(&ctx.db)
                    .await?;
                }
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
pub(crate) fn actor_document(ap: Option<&ApContext>, account: &(String, String)) -> Value {
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
pub(crate) async fn build_archive(
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

    // Statuses → Create activities for authored objects, Announce for
    // boosts (Mastodon's outbox shape — a boost is not an empty Note).
    let statuses = sqlx::query(
        "SELECT id, uri, content, content_html, spoiler_text, sensitive, language, visibility, \
                in_reply_to_uri, quote_uri, reblog_of_id, \
                created_at, original_created_at FROM statuses \
         WHERE account_id = $1 AND NOT tombstone ORDER BY created_at, id",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let followers_url = format!("{actor_id}/followers");

    // Referenced data the outbox objects carry: hashtags, mentions,
    // attachments (with archive-relative URLs for embedded bytes) and
    // polls.
    let status_ids: Vec<i64> = statuses.iter().map(|s| s.get::<i64, _>("id")).collect();
    let mut tag_map: std::collections::HashMap<i64, Vec<Value>> = std::collections::HashMap::new();
    let mut mention_map: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    let mut attach_map: std::collections::HashMap<i64, Vec<Value>> =
        std::collections::HashMap::new();
    if !status_ids.is_empty() {
        for r in sqlx::query(
            "SELECT st.status_id, t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
             WHERE st.status_id = ANY($1) ORDER BY st.status_id, t.name",
        )
        .bind(&status_ids)
        .fetch_all(db)
        .await?
        {
            tag_map
                .entry(r.get::<i64, _>("status_id"))
                .or_default()
                .push(json!({
                    "type": "Hashtag",
                    "name": format!("#{}", r.get::<String, _>("name")),
                }));
        }
        for r in sqlx::query(
            "SELECT sm.status_id, a.actor_id FROM status_mentions sm \
             JOIN accounts a ON a.id = sm.account_id \
             WHERE sm.status_id = ANY($1) ORDER BY sm.status_id, sm.account_id",
        )
        .bind(&status_ids)
        .fetch_all(db)
        .await?
        {
            mention_map
                .entry(r.get::<i64, _>("status_id"))
                .or_default()
                .push(r.get::<String, _>("actor_id"));
        }
        for r in sqlx::query(
            "SELECT id, status_id, remote_url, file_path, content_type, description \
             FROM media_attachments WHERE status_id = ANY($1) ORDER BY id",
        )
        .bind(&status_ids)
        .fetch_all(db)
        .await?
        {
            let att_id: i64 = r.get("id");
            let status_id: i64 = r.get("status_id");
            let remote_url: Option<String> = r.get("remote_url");
            let file_path: Option<String> = r.get("file_path");
            let url = match remote_url {
                Some(u) => u,
                None => match &file_path {
                    Some(key) => media_member_name(att_id, key),
                    None => format!("media_attachments/files/{att_id}"),
                },
            };
            attach_map.entry(status_id).or_default().push(json!({
                "type": "Document",
                "mediaType": r.get::<Option<String>, _>("content_type"),
                "url": url,
                "name": r.get::<Option<String>, _>("description"),
            }));
        }
    }
    let mut poll_map: std::collections::HashMap<i64, (bool, Option<String>, Vec<Value>)> =
        std::collections::HashMap::new();
    if !status_ids.is_empty() {
        for p in sqlx::query(
            "SELECT id, status_id, expires_at, multiple FROM polls \
             WHERE status_id = ANY($1)",
        )
        .bind(&status_ids)
        .fetch_all(db)
        .await?
        {
            let poll_id: i64 = p.get("id");
            let options = sqlx::query(
                "SELECT idx, title, votes_count FROM poll_options \
                 WHERE poll_id = $1 ORDER BY idx",
            )
            .bind(poll_id)
            .fetch_all(db)
            .await?;
            let opts = options
                .iter()
                .map(|o| {
                    json!({
                        "type": "Note",
                        "name": o.get::<String, _>("title"),
                        "replies": {
                            "type": "Collection",
                            "totalItems": o.get::<i32, _>("votes_count"),
                        },
                    })
                })
                .collect();
            poll_map.insert(
                p.get::<i64, _>("status_id"),
                (
                    p.get::<bool, _>("multiple"),
                    p.get::<Option<DateTime<Utc>>, _>("expires_at").map(ap_time),
                    opts,
                ),
            );
        }
    }

    let mut items = Vec::with_capacity(statuses.len());
    for s in &statuses {
        let status_id: i64 = s.get("id");
        let uri: Option<String> = s.get("uri");
        let visibility: String = s.get("visibility");
        let (mut to, cc) = visibility_audience(&visibility, &followers_url);
        // Direct posts carry their recipients (the mention set) —
        // Mastodon exports DMs to the mentioned actors.
        if visibility == "direct" {
            to = mention_map.get(&status_id).cloned().unwrap_or_default();
        }
        let published: DateTime<Utc> = s
            .get::<Option<DateTime<Utc>>, _>("original_created_at")
            .or_else(|| s.get::<Option<DateTime<Utc>>, _>("created_at"))
            .unwrap_or_else(Utc::now);
        if let Some(reblog_of) = s.get::<Option<i64>, _>("reblog_of_id") {
            let object_uri: Option<String> =
                sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
                    .bind(reblog_of)
                    .fetch_optional(db)
                    .await?
                    .flatten();
            items.push(json!({
                "id": format!("{}#announce", uri.clone().unwrap_or_default()),
                "type": "Announce",
                "actor": actor_id,
                "published": ap_time(published),
                "to": to,
                "cc": cc,
                "object": object_uri,
            }));
            continue;
        }
        let object_id = uri.clone().unwrap_or_default();
        let content = s
            .get::<Option<String>, _>("content_html")
            .unwrap_or_else(|| s.get::<String, _>("content"));
        let poll = poll_map.get(&status_id);
        let mut object = json!({
            "id": object_id,
            "type": if poll.is_some() { "Question" } else { "Note" },
            "attributedTo": actor_id,
            "content": content,
            "summary": s.get::<Option<String>, _>("spoiler_text"),
            "sensitive": s.get::<bool, _>("sensitive"),
            "published": ap_time(published),
            "to": to,
            "cc": cc,
        });
        if let Some(lang) = s.get::<Option<String>, _>("language") {
            object["contentMap"] = json!({ lang: content });
        }
        if let Some(parent) = s.get::<Option<String>, _>("in_reply_to_uri") {
            object["inReplyTo"] = json!(parent);
        }
        if let Some(quote) = s.get::<Option<String>, _>("quote_uri") {
            object["quoteUrl"] = json!(quote);
        }
        let mut tags = tag_map.get(&status_id).cloned().unwrap_or_default();
        if let Some(mentions) = mention_map.get(&status_id) {
            for href in mentions {
                tags.push(json!({ "type": "Mention", "href": href }));
            }
        }
        if !tags.is_empty() {
            object["tag"] = json!(tags);
        }
        if let Some(atts) = attach_map.get(&status_id) {
            if !atts.is_empty() {
                object["attachment"] = json!(atts);
            }
        }
        if let Some((multiple, expires, options)) = poll {
            object[if *multiple { "anyOf" } else { "oneOf" }] = json!(options);
            if let Some(end) = expires {
                object["endTime"] = json!(end);
            }
        }
        items.push(json!({
            "id": format!("{object_id}#create"),
            "type": "Create",
            "actor": actor_id,
            "published": ap_time(published),
            "to": to,
            "cc": cc,
            "object": object,
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
            serde_json::to_vec(&json!({
                "id": format!("{actor_id}/likes"),
                "type": "OrderedCollection",
                "totalItems": likes.len(),
                "orderedItems": likes,
            }))
            .map_err(|e| Error::Internal(e.to_string()))?,
        ),
        (
            "bookmarks.json".into(),
            serde_json::to_vec(&json!({
                "id": format!("{actor_id}/bookmarks"),
                "type": "OrderedCollection",
                "totalItems": bookmarks.len(),
                "orderedItems": bookmarks,
            }))
            .map_err(|e| Error::Internal(e.to_string()))?,
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
    let mut missing: Vec<String> = Vec::new();
    if let Some((avatar, banner)) = image_paths {
        for (key, name) in [(avatar, "avatar"), (banner, "header")] {
            let Some(key) = key else { continue };
            match ctx.media.get(&key).await {
                Ok(bytes) => {
                    let ext = key
                        .rsplit_once('.')
                        .map(|(_, e)| e)
                        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
                        .unwrap_or("png");
                    members.push((format!("{name}.{ext}"), bytes));
                }
                Err(e @ Error::NotFound(_)) => {
                    tracing::warn!(account_id, key = %key, error = %e, "export profile image missing");
                    missing.push(key);
                }
                Err(e) => return Err(e),
            }
        }
    }

    // Media attachment bytes under media_attachments/files/. Missing
    // objects are reported, not silently dropped — the outbox still
    // references the member, so the archive carries the list.
    let media = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, file_path FROM media_attachments \
         WHERE account_id = $1 AND file_path IS NOT NULL",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    for (att_id, key) in media {
        match ctx.media.get(&key).await {
            Ok(bytes) => {
                // One member per attachment row — two rows sharing a
                // content-addressed key still get distinct names, so
                // every outbox reference resolves (duplicate bytes in
                // the zip are cheap; a dangling URL is not).
                members.push((media_member_name(att_id, &key), bytes));
            }
            Err(e @ Error::NotFound(_)) => {
                tracing::warn!(account_id, key = %key, error = %e, "export media object missing");
                missing.push(key);
            }
            Err(e) => return Err(e),
        }
    }
    if !missing.is_empty() {
        members.push((MISSING_MEDIA_FILE.into(), missing.join("\n").into_bytes()));
    }

    let bytes = tokio::task::spawn_blocking(move || {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        // First writer wins on a duplicate member name — the importer
        // rejects duplicates outright, so the archive must never
        // contain one.
        let mut names = std::collections::HashSet::new();
        for (name, data) in members.iter() {
            if !names.insert(name.as_str()) {
                continue;
            }
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
pub(crate) const STUCK_RUNNING_HOURS: i64 = 24;

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
