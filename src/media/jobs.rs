//! Media job handlers — remote-attachment caching (`RemoteMediaFetch`)
//! and the cached-remote-media sweep (`CleanupRemoteMedia`), epic 18.
//!
//! `RemoteMediaFetch` carries `{media_id}`: fetch the row's
//! `remote_url` through pubby's SSRF-guarded `HttpClient`, validate and
//! store the bytes through the configured [`MediaStore`], and point
//! `file_path` at the stored key (`remote_url` stays as provenance —
//! cleanup clears `file_path` and serving falls back to it).
//!
//! `CleanupRemoteMedia` carries no payload; it sweeps cached remote
//! media past `media.remote_cleanup_max_age_days` or over
//! `media.remote_cleanup_max_bytes` (oldest first). Media on posts a
//! local user boosted or quoted is never evicted.

use std::collections::HashMap;

use serde_json::Value;

use crate::{
    jobs::{JobContext, JobHandler, JobKind},
    Error, Result,
};

/// Sweep batch size — matches `posts::jobs::SWEEP_BATCH`.
const SWEEP_BATCH: i64 = 200;

/// The metadata a fetched remote attachment writes back to its row.
/// `pub` so the conditional write-back is directly testable
/// ([`persist_remote_media`]).
pub struct FetchedMedia {
    /// Storage key the bytes were just written under.
    pub key: String,
    /// Attachment `type` (`image`/`video`/`audio`).
    pub kind: &'static str,
    /// Stored content type (post-sniff, not the remote's claim).
    pub content_type: String,
    /// Stored byte size.
    pub size: i64,
    /// Blurhash for images.
    pub blurhash: Option<String>,
    /// Pixel width for images.
    pub width: Option<i32>,
    /// Pixel height for images.
    pub height: Option<i32>,
    /// Whether the shared image pipeline ran.
    pub processed: bool,
    /// Whether metadata was stripped.
    pub exif_stripped: bool,
}

/// Claim `media_id`'s row for `media` — the `file_path IS NULL` guard
/// keeps a duplicate delivery (or a row deleted mid-fetch) from
/// stealing the write. Returns `false` when no row was updated: the
/// object just stored under `media.key` is then unreferenced, so it
/// is deleted here rather than orphaned in the backend. A failed
/// UPDATE re-reads the row first — an `Err` does not prove the write
/// lost (a dropped connection after Postgres commits surfaces as an
/// error too), so the object is dropped only when it is provably
/// unreferenced. A recoverable leak beats a permanently broken
/// `file_path` that serving would prefer over `remote_url`.
pub async fn persist_remote_media(
    ctx: &JobContext,
    media_id: i64,
    media: &FetchedMedia,
) -> Result<bool> {
    let updated = match sqlx::query(
        "UPDATE media_attachments \
         SET file_path = $2, type = $3, content_type = $4, file_size = $5, \
             blurhash = $6, width = $7, height = $8, processed = $9, \
             exif_stripped = $10, updated_at = now() \
         WHERE id = $1 AND file_path IS NULL",
    )
    .bind(media_id)
    .bind(&media.key)
    .bind(media.kind)
    .bind(&media.content_type)
    .bind(media.size)
    .bind(&media.blurhash)
    .bind(media.width)
    .bind(media.height)
    .bind(media.processed)
    .bind(media.exif_stripped)
    .execute(&ctx.db)
    .await
    {
        Ok(res) => res.rows_affected(),
        Err(e) => {
            match sqlx::query_scalar::<_, Option<String>>(
                "SELECT file_path FROM media_attachments WHERE id = $1",
            )
            .bind(media_id)
            .fetch_optional(&ctx.db)
            .await
            {
                // The row already references this key — the UPDATE
                // committed; keep the object.
                Ok(Some(Some(key))) if key == media.key => {}
                // Row gone, file_path NULL, or pointing elsewhere —
                // the object is provably unreferenced.
                Ok(_) => drop_unreferenced(ctx, &media.key).await,
                // The re-read failed too — the outcome is ambiguous,
                // so leave the object as a recoverable leak.
                Err(_) => {}
            }
            return Err(e.into());
        }
    };
    if updated == 0 {
        drop_unreferenced(ctx, &media.key).await;
        return Ok(false);
    }
    Ok(true)
}

/// The object under `key` ended up unreferenced — the write-back lost
/// the row claim or errored. Best-effort delete so the store doesn't
/// leak objects that no row points at.
async fn drop_unreferenced(ctx: &JobContext, key: &str) {
    if let Err(e) = ctx.media.delete(key).await {
        tracing::warn!(
            key = %key,
            error = %e,
            "unreferenced remote-media object delete failed"
        );
    }
}

/// `JobKind::RemoteMediaFetch` — cache one remote attachment locally.
/// Idempotent: rows already carrying `file_path` are skipped, and the
/// write-back is conditional so concurrent deliveries can't orphan
/// stored objects ([`persist_remote_media`]).
pub struct RemoteMediaFetchJob;

#[async_trait::async_trait]
impl JobHandler for RemoteMediaFetchJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let media_id = payload
            .get("media_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Validation("remote media job lacks media_id".to_string()))?;
        let row = sqlx::query(
            "SELECT remote_url, file_path, content_type, account_id \
             FROM media_attachments WHERE id = $1",
        )
        .bind(media_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some(row) = row else {
            return Ok(());
        };
        use sqlx::Row;
        if row.get::<Option<String>, _>("file_path").is_some() {
            return Ok(());
        }
        let Some(remote_url) = row.get::<Option<String>, _>("remote_url") else {
            return Ok(());
        };

        // Epic 21 defense-in-depth: a `reject_media` block that landed
        // after enqueue still stops the fetch (the enqueue path checks
        // too, but rows enqueued before the block would get through).
        // The policy applies to the *author's* domain — the media URL
        // host may be a CDN that isn't itself blocked.
        let author_domain: Option<String> = sqlx::query_scalar(
            "SELECT a.domain FROM media_attachments m \
             JOIN accounts a ON a.id = m.account_id WHERE m.id = $1",
        )
        .bind(media_id)
        .fetch_optional(&ctx.db)
        .await?
        .flatten();
        if let Some(domain) = author_domain {
            if crate::moderation::blocks::rejects_media(&ctx.db, &domain)
                .await
                .unwrap_or(false)
            {
                tracing::info!(media_id, domain, "remote media fetch skipped: reject_media");
                return Ok(());
            }
        }

        let timeout = std::time::Duration::from_secs(ctx.config.federation.request_timeout_seconds);
        let allow_private = ctx.config.federation.allow_private_targets;
        let url = remote_url.clone();
        let response = crate::activitypub::actors::sync_call(move || {
            let client = if allow_private {
                pubby_sdk::http::HttpClient::new_relaxed(timeout)
            } else {
                pubby_sdk::http::HttpClient::new(timeout)
            };
            client.get(&url, &HashMap::new())
        });
        let response = match response {
            Ok(response) => response,
            Err(e) => return Err(Error::Internal(format!("remote media fetch: {e}"))),
        };
        if !response.is_success() {
            // 404/410 are definitive — the remote will never serve this
            // URL again, so don't burn the retry budget.
            if matches!(response.status, 404 | 410) {
                tracing::info!(
                    media_id,
                    "remote media gone (definitive) — leaving remote_url"
                );
                return Ok(());
            }
            return Err(Error::Internal(format!(
                "remote media fetch: HTTP {}",
                response.status
            )));
        }
        let bytes = response.body;
        if bytes.is_empty() {
            // Possibly transient (truncated proxy response) — retry.
            return Err(Error::Internal("remote media fetch: empty body".into()));
        }
        if bytes.len() as u64 > ctx.config.media.max_upload_size {
            // Definitive: retrying can never shrink the body, so don't
            // burn the retry budget — serving keeps `remote_url`.
            tracing::info!(
                media_id,
                size = bytes.len(),
                "remote media over max_upload_size — leaving remote_url"
            );
            return Ok(());
        }

        // Images go through the shared pipeline (bounded decode, EXIF
        // strip, blurhash); audio/video are stored raw.
        let is_image = image::guess_format(&bytes)
            .map(|f| {
                matches!(
                    f,
                    image::ImageFormat::Png
                        | image::ImageFormat::Jpeg
                        | image::ImageFormat::WebP
                        | image::ImageFormat::Gif
                )
            })
            .unwrap_or(false);
        let (stored, kind, content_type, ext, width, height, blurhash, exif_stripped, processed) =
            if is_image {
                let strip = ctx.config.media.strip_exif;
                let raw = bytes;
                let image = tokio::task::spawn_blocking(move || {
                    super::images::process_image(&raw, strip, "remote media")
                })
                .await
                .map_err(|e| Error::Internal(format!("image task failed: {e}")))??;
                (
                    image.bytes,
                    "image",
                    image.content_type,
                    image.ext.to_string(),
                    Some(image.width as i32),
                    Some(image.height as i32),
                    Some(image.blurhash),
                    strip && image.ext != "gif",
                    true,
                )
            } else {
                let Some((ext, mime, kind)) = super::attachments::sniff_media(&bytes)
                    .map(|(e, m, k)| (e.to_string(), m.to_string(), k))
                    .or_else(|| {
                        row.get::<Option<String>, _>("content_type")
                            .as_deref()
                            .and_then(super::attachments::for_declared)
                    })
                else {
                    // The remote served something that isn't media
                    // (HTML error page, opaque payload) — retrying never
                    // helps, so treat it as definitive like 404/410.
                    tracing::info!(
                        media_id,
                        "remote media is not image/video/audio — leaving remote_url"
                    );
                    return Ok(());
                };
                (bytes, kind, mime, ext, None, None, None, false, false)
            };
        let account_id: i64 = row.get("account_id");
        let key = format!(
            "{}/{account_id}/{}.{}",
            super::attachments::PREFIX,
            uuid::Uuid::new_v4(),
            ext
        );
        ctx.media.put(&key, &stored, &content_type).await?;
        let media = FetchedMedia {
            key,
            kind,
            content_type,
            size: stored.len() as i64,
            blurhash,
            width,
            height,
            processed,
            exif_stripped,
        };
        persist_remote_media(ctx, media_id, &media).await?;
        Ok(())
    }
}

/// `JobKind::CleanupRemoteMedia` — evict cached remote media past the
/// age threshold, or oldest-first until total cached bytes sit under
/// `remote_cleanup_max_bytes`. Attachments on posts a local user
/// boosted or quoted are never evicted; the row keeps `remote_url`
/// and only `file_path` is cleared.
pub struct CleanupRemoteMediaJob;

#[async_trait::async_trait]
impl JobHandler for CleanupRemoteMediaJob {
    async fn run(&self, ctx: &JobContext, _payload: &Value) -> Result<()> {
        // Candidates: cached rows owned by a remote account, minus the
        // protected set (status boosted or quoted by a local user). The
        // scan is bounded — quota math uses a separate SUM over the same
        // predicate so a large cache doesn't load every candidate.
        const CANDIDATES: &str = "FROM media_attachments m \
             JOIN accounts a ON a.id = m.account_id \
             WHERE m.file_path IS NOT NULL AND a.domain IS NOT NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM statuses b \
                   JOIN accounts ba ON ba.id = b.account_id \
                   WHERE (b.reblog_of_id = m.status_id OR b.quote_of_id = m.status_id) \
                     AND ba.domain IS NULL)";
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(SUM(m.file_size), 0)::bigint {CANDIDATES}"
        ))
        .fetch_one(&ctx.db)
        .await?;
        let rows: Vec<(i64, String, i64, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as(&format!(
                "SELECT m.id, m.file_path, COALESCE(m.file_size, 0), m.created_at \
             {CANDIDATES} ORDER BY m.created_at LIMIT {SWEEP_BATCH}"
            ))
            .fetch_all(&ctx.db)
            .await?;
        let max_age = chrono::Duration::days(ctx.config.media.remote_cleanup_max_age_days as i64);
        let now = chrono::Utc::now();
        // Evict: everything older than max_age, plus oldest-first
        // extras while the cache exceeds the byte ceiling. The `u64`
        // ceiling saturates at `i64::MAX` — a plain `as` cast would
        // wrap negative and evict everything.
        let max_bytes =
            i64::try_from(ctx.config.media.remote_cleanup_max_bytes).unwrap_or(i64::MAX);
        let mut over = total.saturating_sub(max_bytes);
        let mut evicted = 0usize;
        for (id, key, size, created_at) in rows {
            let too_old = now - created_at > max_age;
            let over_quota = over > 0;
            if !too_old && !over_quota {
                continue;
            }
            if let Err(e) = ctx.media.delete(&key).await {
                tracing::warn!(key, error = %e, "remote media object delete failed — row keeps file_path");
                continue;
            }
            sqlx::query("UPDATE media_attachments SET file_path = NULL WHERE id = $1")
                .bind(id)
                .execute(&ctx.db)
                .await?;
            over -= size;
            evicted += 1;
        }
        if evicted > 0 {
            tracing::info!(evicted, "remote media cleanup");
        }
        Ok(())
    }
}

/// `JobKind::DeleteMediaObject` — remove one stored profile-image
/// object (`{key}`) once no `accounts` row references it. Producers
/// enqueue it delayed: replaced avatar/header URLs were served
/// `immutable` for a year and remote servers may not have processed
/// the actor `Update` yet, so the grace window keeps the old URL
/// alive. The reference check re-runs at execution, so a key the row
/// regained during the window is never deleted.
pub struct DeleteMediaObjectJob;

#[async_trait::async_trait]
impl JobHandler for DeleteMediaObjectJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let key = payload
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Validation("delete-media job lacks key".to_string()))?;
        // Profile images are the only producers — refuse any other
        // prefix so a malformed payload can't delete arbitrary objects.
        if !key.starts_with("avatars/") && !key.starts_with("headers/") {
            tracing::warn!(key, "delete-media job got a non-profile key — skipping");
            return Ok(());
        }
        let referenced: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM accounts \
             WHERE avatar_path = $1 OR banner_path = $1)",
        )
        .bind(key)
        .fetch_one(&ctx.db)
        .await?;
        if referenced {
            return Ok(());
        }
        ctx.media.delete(key).await
    }
}

/// Register this module's handlers (called from `cli/mod.rs` after the
/// stub set — real handlers replace the stubs).
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::RemoteMediaFetch, RemoteMediaFetchJob);
    registry.register(JobKind::CleanupRemoteMedia, CleanupRemoteMediaJob);
    registry.register(JobKind::DeleteMediaObject, DeleteMediaObjectJob);
}
