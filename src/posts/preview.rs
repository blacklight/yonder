//! Link previews — first-URL extraction, the `FetchLinkPreview` job,
//! Open Graph parsing and the Redis metadata cache (epic 12).
//!
//! Rules: fetch only when `content.link_previews` is on, the status
//! has **no media attachments**, and the source carries at least one
//! allowed URL — and only the **first** one. Own-instance URLs and
//! non-`content.valid_url_schemes` schemes are skipped. Fetches go
//! through pubby's SSRF-guarded `HttpClient`; parsed metadata caches
//! in Redis for `content.link_preview_cache_hours`. Every failure
//! acks (`Ok(())`) — a failed preview leaves no card and never
//! retry-storms.

use std::collections::HashMap;

use scraper::{Html, Selector};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    activitypub::ApContext,
    jobs::{JobContext, JobHandler, JobKind},
    Error, Result,
};

use super::{PostContext, StatusRow};

/// HTML document cap — the pubby client already bounds bodies, this
/// trims what the parser sees.
const MAX_DOC: usize = 1024 * 1024;
/// Card image cap.
const MAX_IMAGE: u64 = 5 * 1024 * 1024;

/// Enqueue `FetchLinkPreview` when the status qualifies (no media,
/// previews enabled). Best-effort — enqueue failures only log.
pub async fn enqueue(ctx: &PostContext, status: &StatusRow, media_ids: &[i64]) {
    if !ctx.config.content.link_previews || !media_ids.is_empty() {
        return;
    }
    if extract_first_url(ctx, &status.content).is_none() {
        return;
    }
    if let Err(e) = ctx
        .dispatcher
        .enqueue(JobKind::FetchLinkPreview, json!({"status_id": status.id}))
        .await
    {
        tracing::warn!(status_id = status.id, error = %e, "preview enqueue failed");
    }
}

/// The first allowed, non-own-instance URL in `source` (trailing
/// punctuation stripped). `None` → no preview.
pub fn extract_first_url(ctx: &PostContext, source: &str) -> Option<String> {
    let base = ctx.base_url();
    let domain = ctx.config.instance.domain.to_lowercase();
    for m in url_scan(source) {
        let (url, _) = super::render::split_url_trailing(m);
        let lower = url.to_lowercase();
        if !super::render::scheme_allowed(url, &ctx.config) {
            continue;
        }
        if lower.starts_with(&base.to_lowercase())
            || lower.starts_with(&format!("http://{domain}"))
            || lower.starts_with(&format!("https://{domain}"))
        {
            continue;
        }
        return Some(url.to_string());
    }
    None
}

/// Yield raw `http(s)://…` spans (pre-trailing-punctuation).
fn url_scan(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(pos) = rest.find("http") {
        let tail = &rest[pos..];
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let end = tail
                .find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"' || c == '\'')
                .unwrap_or(tail.len());
            out.push(&tail[..end]);
            rest = &tail[end..];
        } else {
            rest = &tail[4.min(tail.len())..];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------

/// `JobKind::FetchLinkPreview` — `{status_id}`; never fails (a failed
/// fetch/parse is logged and acked, not retried).
pub struct FetchLinkPreviewJob {
    ap: Option<ApContext>,
}

impl FetchLinkPreviewJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        FetchLinkPreviewJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for FetchLinkPreviewJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(status_id) = payload.get("status_id").and_then(Value::as_i64) else {
            return Ok(());
        };
        let post_ctx = PostContext::from_job(ctx, self.ap.clone());
        if let Err(e) = fetch_card(&post_ctx, status_id).await {
            tracing::debug!(status_id, error = %e, "link preview fetch failed");
        }
        Ok(())
    }
}

/// The cache envelope stored under `cache:preview:{hash}`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CardMeta {
    title: String,
    description: String,
    image: Option<String>,
    author_name: String,
    author_url: String,
    provider_name: String,
    provider_url: String,
}

fn cache_key(url: &str) -> String {
    format!(
        "preview:{}",
        &hex::encode(Sha256::digest(url.as_bytes()))[..32]
    )
}

async fn fetch_card(ctx: &PostContext, status_id: i64) -> Result<()> {
    let Some(status) = super::load(&ctx.db, status_id).await? else {
        return Ok(());
    };
    // Media-attached statuses never get a card; a card already present
    // (edit re-run) short-circuits.
    let has_media: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_attachments WHERE status_id = $1)")
            .bind(status_id)
            .fetch_one(&ctx.db)
            .await?;
    if has_media {
        return Ok(());
    }
    let Some(url) = extract_first_url(ctx, &status.content) else {
        return Ok(());
    };

    let cache = crate::cache::RedisCache::new(ctx.redis.clone());
    let ttl =
        std::time::Duration::from_secs(ctx.config.content.link_preview_cache_hours as u64 * 3600);
    if let Ok(Some(cached)) = cache.get(&cache_key(&url)).await {
        if let Ok(meta) = serde_json::from_value::<CardMeta>(cached.clone()) {
            let image_path = cached.get("image_path").and_then(|v| v.as_str());
            upsert_card(ctx, status_id, &url, &meta, image_path).await?;
            return Ok(());
        }
    }

    let Some(ap) = &ctx.ap else {
        return Ok(());
    };
    let http = crate::activitypub::delivery::http_client(ap);
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "text/html".to_string());
    headers.insert(
        "User-Agent".to_string(),
        ctx.config.federation.user_agent.clone(),
    );
    let page_url = url.clone();
    let response = crate::activitypub::actors::sync_call(move || http.get(&page_url, &headers))
        .map_err(Error::Federation)?;
    if !response.is_success() {
        return Err(Error::Internal(format!(
            "preview fetch HTTP {}",
            response.status
        )));
    }
    // pubby's HttpResponse drops headers — sniff instead: card
    // metadata only parses out of HTML anyway.
    let body = response.text();
    if !body.trim_start().starts_with('<') {
        return Err(Error::Internal("preview target is not HTML".to_string()));
    }
    // Truncate on a char boundary — a raw byte slice could panic.
    let doc = body.get(..MAX_DOC).unwrap_or(&body);
    let meta = parse_metadata(doc);

    // Optional card image: download, sniff, store under `previews/`.
    let image_key = match meta.image.as_deref() {
        Some(img) => fetch_image(ctx, ap, img, &url).await,
        None => None,
    };
    let meta = CardMeta {
        image: None,
        ..meta
    };
    upsert_card(ctx, status_id, &url, &meta, image_key.as_deref()).await?;

    let mut cached = serde_json::to_value(&meta).map_err(|e| Error::Internal(e.to_string()))?;
    if let Some(key) = &image_key {
        cached["image_path"] = json!(key);
    }
    let _ = cache.set(&cache_key(&url), &cached, ttl).await;
    Ok(())
}

/// Parse Open Graph metadata with `twitter:*` and `<title>`/
/// `<meta name=description>` fallbacks.
fn parse_metadata(doc: &str) -> CardMeta {
    let html = Html::parse_document(doc);
    let og = |name: &str| -> Option<String> {
        let sel = Selector::parse(&format!("meta[property=\"og:{name}\"]")).ok()?;
        html.select(&sel)
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let meta_name = |name: &str| -> Option<String> {
        let sel = Selector::parse(&format!("meta[name=\"{name}\"]")).ok()?;
        html.select(&sel)
            .next()
            .and_then(|e| e.value().attr("content"))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    let title = og("title")
        .or_else(|| {
            let sel = Selector::parse("title").ok()?;
            html.select(&sel).next().map(|e| e.text().collect())
        })
        .unwrap_or_default();
    CardMeta {
        title,
        description: og("description")
            .or_else(|| meta_name("description"))
            .unwrap_or_default(),
        image: og("image").or_else(|| meta_name("twitter:image")),
        author_name: og("article:author")
            .or_else(|| meta_name("author"))
            .unwrap_or_default(),
        author_url: String::new(),
        provider_name: og("site_name").unwrap_or_default(),
        provider_url: String::new(),
    }
}

/// Download and store the card image under `previews/{uuid}.{ext}`;
/// `None` on any failure — the card still renders without it.
async fn fetch_image(
    ctx: &PostContext,
    ap: &ApContext,
    image_url: &str,
    page_url: &str,
) -> Option<String> {
    let resolved = url::Url::parse(page_url)
        .ok()
        .and_then(|base| base.join(image_url).ok())
        .map(|u| u.to_string())?;
    let http = crate::activitypub::delivery::http_client(ap);
    let url = resolved.clone();
    let response =
        crate::activitypub::actors::sync_call(move || http.get(&url, &HashMap::new())).ok()?;
    if !response.is_success() || response.body.len() as u64 > MAX_IMAGE {
        return None;
    }
    let format = image::guess_format(&response.body).ok()?;
    let ext = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Gif => "gif",
        _ => return None,
    };
    let key = format!(
        "{}/{}.{}",
        crate::media::attachments::PREVIEWS_PREFIX,
        uuid::Uuid::new_v4(),
        ext
    );
    let ct = crate::media::images::content_type_for_key(&key).unwrap_or("application/octet-stream");
    ctx.media.put(&key, &response.body, ct).await.ok()?;
    Some(key)
}

/// Upsert the `preview_cards` row (`UNIQUE (status_id, url)`).
async fn upsert_card(
    ctx: &PostContext,
    status_id: i64,
    url: &str,
    meta: &CardMeta,
    image_path: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO preview_cards \
             (status_id, url, title, description, image_path, author_name, author_url, \
              provider_name, provider_url) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
         ON CONFLICT (status_id, url) DO UPDATE SET \
             title = EXCLUDED.title, description = EXCLUDED.description, \
             image_path = EXCLUDED.image_path, author_name = EXCLUDED.author_name, \
             author_url = EXCLUDED.author_url, provider_name = EXCLUDED.provider_name, \
             provider_url = EXCLUDED.provider_url",
    )
    .bind(status_id)
    .bind(url)
    .bind(&meta.title)
    .bind(&meta.description)
    .bind(image_path)
    .bind(&meta.author_name)
    .bind(&meta.author_url)
    .bind(&meta.provider_name)
    .bind(&meta.provider_url)
    .execute(&ctx.db)
    .await?;
    Ok(())
}

/// Register this module's job handler.
pub(crate) fn register(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(
        JobKind::FetchLinkPreview,
        FetchLinkPreviewJob::new(ap.cloned()),
    );
}
