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
//!
//! The enqueue happens everywhere a status is written: local create
//! and edit ([`super::create`]), scheduled publish
//! ([`super::scheduled`]) and remote materialization
//! ([`super::remote`]). Re-runs converge `preview_cards` to the
//! status' current eligibility — a stale card is worse than none.

use std::collections::HashMap;

use scraper::{Html, Selector};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    activitypub::ApContext,
    jobs::{Dispatcher, JobContext, JobHandler, JobKind, Queue},
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

/// `JobKind::FetchLinkPreview` — `{status_id}` refreshes one status;
/// never fails (a failed fetch/parse is logged and acked, not
/// retried). An empty payload is the rescan sweep: it fans out one
/// per-status job per `preview_cards` row whose `scan_version`
/// predates [`CARD_SCAN_VERSION`] — how cards stored before a
/// card-shape change (e.g. video embeds) get re-fetched
/// (`yonder run-job fetch_link_preview`).
pub struct FetchLinkPreviewJob {
    ap: Option<ApContext>,
}

impl FetchLinkPreviewJob {
    /// Create the handler; `ap` is `Some` when federation is enabled.
    pub fn new(ap: Option<ApContext>) -> Self {
        FetchLinkPreviewJob { ap }
    }

    /// Sweep mode: mark one batch of stale card rows as scanned, then
    /// enqueue a per-status job for each on `Queue::Bulk` (the sweep
    /// must not crowd the interactive queue). Marking at enqueue
    /// time — not at fetch time — bounds the fan-out: a page that
    /// keeps failing can't be re-enqueued forever, and a full batch
    /// re-enqueues the sweep itself so the remainder is covered.
    async fn sweep(&self, ctx: &JobContext) -> Result<()> {
        let status_ids: Vec<i64> = sqlx::query_scalar(
            "UPDATE preview_cards SET scan_version = $1 \
             WHERE status_id IN ( \
                 SELECT DISTINCT status_id FROM preview_cards \
                 WHERE scan_version < $1 ORDER BY status_id LIMIT $2 \
             ) \
             RETURNING status_id",
        )
        .bind(CARD_SCAN_VERSION)
        .bind(SWEEP_BATCH_CAP)
        .fetch_all(&ctx.db)
        .await?;
        if status_ids.is_empty() {
            return Ok(());
        }
        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        let mut status_ids = status_ids;
        status_ids.sort_unstable();
        status_ids.dedup();
        let hit_cap = status_ids.len() as i64 >= SWEEP_BATCH_CAP;
        for status_id in status_ids {
            dispatcher
                .enqueue_on(
                    Queue::Bulk,
                    JobKind::FetchLinkPreview,
                    json!({"status_id": status_id}),
                )
                .await?;
        }
        if hit_cap {
            dispatcher
                .enqueue_on(Queue::Bulk, JobKind::FetchLinkPreview, json!({}))
                .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl JobHandler for FetchLinkPreviewJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(status_id) = payload.get("status_id").and_then(Value::as_i64) else {
            return self.sweep(ctx).await;
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
    // `serde(default)` — cache entries written before embeds existed
    // still deserialize.
    #[serde(default)]
    embed_url: String,
    #[serde(default)]
    embed_width: i32,
    #[serde(default)]
    embed_height: i32,
}

/// `preview_cards.scan_version` stamped on every upsert — bump when
/// the stored card shape changes so the sweep can rescan rows
/// written by older code (v1 = video-embed fields).
const CARD_SCAN_VERSION: i32 = 1;

/// Per-run cap on the rescan sweep — one fan-out must not flood the
/// queue unboundedly; a full batch re-enqueues the sweep until no
/// stale rows remain.
const SWEEP_BATCH_CAP: i64 = 1000;

fn cache_key(url: &str) -> String {
    // `v2` — pre-embed envelopes would otherwise serve stale meta
    // for the whole cache TTL and keep stamping `link` cards.
    format!(
        "preview:v2:{}",
        &hex::encode(Sha256::digest(url.as_bytes()))[..32]
    )
}

async fn fetch_card(ctx: &PostContext, status_id: i64) -> Result<()> {
    let Some(status) = super::load(&ctx.db, status_id).await? else {
        return Ok(());
    };
    // Media-attached statuses never get a card. Re-runs (edits, remote
    // Updates) converge the stored card to the status' current
    // eligibility — disqualifying states delete rather than linger.
    let has_media: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM media_attachments WHERE status_id = $1)")
            .bind(status_id)
            .fetch_one(&ctx.db)
            .await?;
    if has_media {
        // Media disqualifies the card — a re-run after an edit added
        // attachments clears whatever a previous run stored.
        clear_cards(&ctx.db, status_id, None).await?;
        return Ok(());
    }
    let Some(url) = extract_first_url(ctx, &status.content) else {
        clear_cards(&ctx.db, status_id, None).await?;
        return Ok(());
    };
    // A re-run after an edit/remote Update may have superseded the
    // first-URL — cards keyed to a different URL are stale.
    clear_cards(&ctx.db, status_id, Some(&url)).await?;

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
    let meta = parse_metadata(doc, &url);

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

/// Embed URL cap — `twitter:player`/`og:video` content is
/// page-controlled, so bound what lands in the database.
const MAX_EMBED_URL: usize = 2048;

/// Parse Open Graph metadata with `twitter:*` and `<title>`/
/// `<meta name=description>` fallbacks. `page_url` resolves
/// relative embed-player URLs.
fn parse_metadata(doc: &str, page_url: &str) -> CardMeta {
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
    // Embeddable players: `twitter:player` is always an iframeable
    // URL per the card spec; `og:video*` doubles for raw media files
    // (`video/mp4`), so only a `text/html` player qualifies.
    let embed_url = meta_name("twitter:player").or_else(|| {
        let is_html_player = og("video:type")
            .map(|t| t.eq_ignore_ascii_case("text/html"))
            .unwrap_or(false);
        is_html_player
            .then(|| {
                og("video:secure_url")
                    .or_else(|| og("video:url"))
                    .or_else(|| og("video"))
            })
            .flatten()
    });
    let embed_url = embed_url.and_then(|u| normalize_embed_url(&u, page_url));
    let dim = |name: &str| -> i32 {
        name.parse::<i32>()
            .ok()
            .filter(|n| (1..=100_000).contains(n))
            .unwrap_or(0)
    };
    let player_dim = |axis: &str| -> i32 {
        meta_name(&format!("twitter:player:{axis}"))
            .as_deref()
            .map(dim)
            .filter(|d| *d > 0)
            .or_else(|| og(&format!("video:{axis}")).as_deref().map(dim))
            .unwrap_or(0)
    };
    let (embed_width, embed_height) = if embed_url.is_some() {
        (player_dim("width"), player_dim("height"))
    } else {
        (0, 0)
    };
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
        embed_url: embed_url.unwrap_or_default(),
        embed_width,
        embed_height,
    }
}

/// Validate an embed-player URL: resolve against the page, require
/// `http(s)` (the iframe `src` sink), bound the length. `None`
/// disqualifies — the card stays a plain link.
fn normalize_embed_url(raw: &str, page_url: &str) -> Option<String> {
    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => url::Url::parse(page_url).ok()?.join(raw).ok()?,
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let s = parsed.to_string();
    (s.chars().count() <= MAX_EMBED_URL).then_some(s)
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

/// Converge `preview_cards` to the current run's verdict: `None`
/// clears every card for the status (media attached / no eligible
/// URL), `Some(url)` keeps only the row for the URL this run
/// extracted. Only the disqualifying paths call this — fetch
/// failures keep the previous card.
async fn clear_cards(db: &sqlx::PgPool, status_id: i64, keep_url: Option<&str>) -> Result<()> {
    sqlx::query(
        "DELETE FROM preview_cards \
         WHERE status_id = $1 AND ($2::text IS NULL OR url <> $2)",
    )
    .bind(status_id)
    .bind(keep_url)
    .execute(db)
    .await?;
    Ok(())
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
              provider_name, provider_url, embed_url, embed_width, embed_height, \
              scan_version) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
         ON CONFLICT (status_id, url) DO UPDATE SET \
             title = EXCLUDED.title, description = EXCLUDED.description, \
             image_path = EXCLUDED.image_path, author_name = EXCLUDED.author_name, \
             author_url = EXCLUDED.author_url, provider_name = EXCLUDED.provider_name, \
             provider_url = EXCLUDED.provider_url, embed_url = EXCLUDED.embed_url, \
             embed_width = EXCLUDED.embed_width, embed_height = EXCLUDED.embed_height, \
             scan_version = EXCLUDED.scan_version",
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
    .bind(&meta.embed_url)
    .bind(meta.embed_width)
    .bind(meta.embed_height)
    .bind(CARD_SCAN_VERSION)
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
