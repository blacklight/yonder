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

use std::collections::{HashMap, HashSet};

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

/// Mastodon version token in the preview-fetch UA — see
/// [`preview_user_agent`].
const MASTODON_COMPAT_VERSION: &str = "4.3.0";

/// User-Agent for page and card-image fetches. Providers like
/// YouTube serve a stripped shell page — no `og:video`/
/// `twitter:player` player tags, no oEmbed discovery link, generic
/// `- YouTube` title — to agents they don't allowlist, and they
/// gate on a plain `Mastodon` substring match. The preview fetcher
/// therefore leads with the real configured product/version plus a
/// Mastodon compatibility token (the `(compatible; …)` convention
/// browsers use for `Mozilla`). Federation deliveries keep the
/// plain `federation.user_agent`.
fn preview_user_agent(configured: &str, base_url: &str) -> String {
    if configured.to_lowercase().contains("mastodon") {
        configured.to_string()
    } else {
        format!("{configured} (compatible; Mastodon/{MASTODON_COMPAT_VERSION}; +{base_url})")
    }
}

/// Enqueue `FetchLinkPreview` when the status qualifies (no media,
/// previews enabled). Best-effort — enqueue failures only log.
pub async fn enqueue(ctx: &PostContext, status: &StatusRow, media_ids: &[i64]) {
    if !ctx.config.content.link_previews
        || !media_ids.is_empty()
        || !status.content.contains("http")
    {
        return;
    }
    let excl = link_exclusions(&ctx.db, status.id)
        .await
        .unwrap_or_default();
    if extract_first_url(ctx, &status.content, &excl).is_none() {
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
/// punctuation stripped). Mention and hashtag link targets are
/// excluded — a mention at the top of a post must never produce a
/// profile-page card (Mastodon skips `mention`/`hashtag` anchors the
/// same way). `None` → no preview.
pub fn extract_first_url(ctx: &PostContext, source: &str, excl: &LinkExclusions) -> Option<String> {
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
        if url_excluded(url, excl) {
            continue;
        }
        return Some(url.to_string());
    }
    None
}

/// Mention/tag references whose targets never qualify for a card —
/// built from the status' `status_mentions`/`status_tags` rows. With
/// href-only sanitized HTML the entities are the reliable markers.
#[derive(Debug, Default)]
pub struct LinkExclusions {
    /// Normalized `host/path` (lowercase, trailing `/` trimmed) of
    /// every mentioned account's `url` and `actor_id`.
    urls: std::collections::HashSet<String>,
    /// `(lowercase username, lowercase domain)` — catches mention
    /// links whose path shape differs from the stored profile URL
    /// (`/@alice` vs `/users/alice` on the same host).
    handles: std::collections::HashSet<(String, String)>,
    /// Lowercased status tag names.
    tags: std::collections::HashSet<String>,
}

/// Mention + tag entities for `status_id`, as link exclusions.
async fn link_exclusions(db: &sqlx::PgPool, status_id: i64) -> Result<LinkExclusions> {
    let mentions = sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(
        "SELECT a.username, a.domain, a.url, a.actor_id FROM accounts a \
         JOIN status_mentions m ON m.account_id = a.id WHERE m.status_id = $1",
    )
    .bind(status_id)
    .fetch_all(db)
    .await?;
    let tags: Vec<String> = sqlx::query_scalar(
        "SELECT t.name FROM tags t JOIN status_tags s ON s.tag_id = t.id \
         WHERE s.status_id = $1",
    )
    .bind(status_id)
    .fetch_all(db)
    .await?;
    let mut excl = LinkExclusions::default();
    for (username, domain, url, actor_id) in mentions {
        if let Some(d) = domain {
            excl.handles
                .insert((username.to_lowercase(), d.to_lowercase()));
        }
        for u in [url, Some(actor_id)].into_iter().flatten() {
            if let Ok(p) = url::Url::parse(&u) {
                excl.urls.insert(normalize_url(&p));
            }
        }
    }
    excl.tags = tags.into_iter().map(|t| t.to_lowercase()).collect();
    Ok(excl)
}

/// `host/path` lowercased, trailing slashes stripped — scheme-agnostic
/// so `http`/`https` forms of the same target compare equal.
fn normalize_url(u: &url::Url) -> String {
    format!(
        "{}{}",
        u.host_str().unwrap_or_default().to_lowercase(),
        u.path().trim_end_matches('/')
    )
}

/// Percent-decode one path segment (`+` stays literal — path, not query).
fn decode_segment(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&seg[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `url` points at one of the status' mentioned accounts or
/// hashtags — matched by stored entity URL, by `@user`/`user` path
/// basename on the mention's domain, or by a tag-shaped
/// `/tags|tag/<name>` path or `?tag=<name>` query naming a status tag.
fn url_excluded(url: &str, excl: &LinkExclusions) -> bool {
    if excl.urls.is_empty() && excl.handles.is_empty() && excl.tags.is_empty() {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if excl.urls.contains(&normalize_url(&parsed)) {
        return true;
    }
    let host = parsed.host_str().unwrap_or_default().to_lowercase();
    let basename = decode_segment(
        parsed
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default(),
    )
    .trim_start_matches('@')
    .to_lowercase();
    if !basename.is_empty() && excl.handles.contains(&(basename, host)) {
        return true;
    }
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    let tag_name = segs
        .windows(2)
        .find_map(|w| {
            (w[0].eq_ignore_ascii_case("tags") || w[0].eq_ignore_ascii_case("tag"))
                .then(|| decode_segment(w[1]).to_lowercase())
        })
        .or_else(|| {
            parsed
                .query_pairs()
                .find(|(k, _)| k == "tag")
                .map(|(_, v)| v.to_lowercase())
        });
    tag_name
        .map(|n| !n.is_empty() && excl.tags.contains(&n))
        .unwrap_or(false)
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
    /// A second phase repairs remote `content` flattened before
    /// anchors emitted their hrefs — see [`Self::repair_content`].
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
        let dispatcher = Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        let mut status_ids = status_ids;
        status_ids.sort_unstable();
        status_ids.dedup();
        let mut hit_cap = status_ids.len() as i64 >= SWEEP_BATCH_CAP;
        for status_id in status_ids {
            dispatcher
                .enqueue_on(
                    Queue::Bulk,
                    JobKind::FetchLinkPreview,
                    json!({"status_id": status_id}),
                )
                .await?;
        }
        hit_cap |= self.repair_content(ctx, &dispatcher).await?;
        if hit_cap {
            dispatcher
                .enqueue_on(Queue::Bulk, JobKind::FetchLinkPreview, json!({}))
                .await?;
        }
        Ok(())
    }

    /// Remote statuses flattened before [`render::html_to_text`]
    /// emitted anchor hrefs carry a `scheme:// <rest>` signature —
    /// Mastodon's invisible-span link markup split the URL across
    /// text nodes, and the href never reached `content`, so no card
    /// was ever fetched (and URL/mention search can't match the
    /// broken text). Re-flatten from `content_html` — the hrefs
    /// survive there — and re-run the fetch on the repaired text.
    /// Candidates must contain an `http(s)` href or the re-flatten
    /// could never change the row; locals are skipped (their
    /// `content` is the author's source text, not a flatten).
    /// Returns `true` when the batch was full and at least one row
    /// changed — an unchanging full batch is permanently broken and
    /// must not re-enqueue the sweep forever.
    async fn repair_content(&self, ctx: &JobContext, dispatcher: &Dispatcher) -> Result<bool> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT id, content, content_html FROM statuses \
             WHERE NOT local AND NOT tombstone \
               AND content_html <> '' \
               AND content ~ 'https?://\\s' \
               AND content_html ~ 'href=\"https?://' \
             ORDER BY id LIMIT $1",
        )
        .bind(SWEEP_BATCH_CAP)
        .fetch_all(&ctx.db)
        .await?;
        let hit_cap = rows.len() as i64 >= SWEEP_BATCH_CAP;
        let mut changed = 0usize;
        for (status_id, _, content_html) in rows {
            let repaired = super::render::html_to_text(&content_html);
            let updated =
                sqlx::query("UPDATE statuses SET content = $2 WHERE id = $1 AND content <> $2")
                    .bind(status_id)
                    .bind(&repaired)
                    .execute(&ctx.db)
                    .await?
                    .rows_affected()
                    > 0;
            if updated {
                changed += 1;
                dispatcher
                    .enqueue_on(
                        Queue::Bulk,
                        JobKind::FetchLinkPreview,
                        json!({"status_id": status_id}),
                    )
                    .await?;
            }
        }
        Ok(hit_cap && changed > 0)
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
/// the stored card shape or candidate rules change so the sweep can
/// rescan rows written by older code (v1 = video-embed fields,
/// v2 = mention/hashtag links excluded from candidates — mention
/// cards are cleared and any later eligible link is re-fetched;
/// v3 = Mastodon-compat fetch UA — cards stored from providers'
/// stripped bot pages are re-fetched for real metadata/embeds;
/// v4 = iframeable-player validation — cards that stored direct
/// media URLs as `twitter:player`/`og:video` embeds are re-fetched).
const CARD_SCAN_VERSION: i32 = 4;

/// Per-run cap on the rescan sweep — one fan-out must not flood the
/// queue unboundedly; a full batch re-enqueues the sweep until no
/// stale rows remain.
const SWEEP_BATCH_CAP: i64 = 1000;

fn cache_key(url: &str) -> String {
    // `v4` — envelopes cached before player-URL validation can carry
    // direct media URLs as embed players.
    format!(
        "preview:v4:{}",
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
    let excl = link_exclusions(&ctx.db, status_id)
        .await
        .unwrap_or_default();
    let Some(url) = extract_first_url(ctx, &status.content, &excl) else {
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
        preview_user_agent(&ctx.config.federation.user_agent, &ctx.base_url()),
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
    let values = |attr: &str, name: &str| -> Vec<String> {
        let Some(sel) = Selector::parse(&format!("meta[{attr}=\"{name}\"]")).ok() else {
            return Vec::new();
        };
        html.select(&sel)
            .filter_map(|e| e.value().attr("content"))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect()
    };
    let og_values = |name: &str| values("property", &format!("og:{name}"));
    let named_values = |name: &str| values("name", name);
    let og = |name: &str| og_values(name).into_iter().next();
    let meta_name = |name: &str| named_values(name).into_iter().next();
    let title = og("title")
        .or_else(|| {
            let sel = Selector::parse("title").ok()?;
            html.select(&sel).next().map(|e| e.text().collect())
        })
        .unwrap_or_default();
    // Card images are collected before choosing a player: broken
    // implementations (notably Akkoma) point `twitter:player` at the
    // same attachment URLs they list as `og:image`.
    let image_urls: HashSet<String> = og_values("image")
        .into_iter()
        .chain(named_values("twitter:image"))
        .filter_map(|u| normalize_embed_url(&u, page_url))
        .collect();
    // Embeddable players: `twitter:player` counts for player cards and
    // undeclared card types, but not when the page only declares a
    // non-player card (Akkoma emits one per image while still saying
    // `summary_large_image`); `og:video*` doubles for raw media files
    // (`video/mp4`), so only `text/html` qualifies.
    let twitter_cards = named_values("twitter:card");
    let player_card = twitter_cards
        .iter()
        .any(|c| matches!(c.to_ascii_lowercase().as_str(), "player" | "player_card"));
    let non_player_card = twitter_cards.iter().any(|c| {
        matches!(
            c.to_ascii_lowercase().as_str(),
            "summary" | "summary_large_image" | "app"
        )
    });
    let mut embed_candidates = if player_card || !non_player_card {
        named_values("twitter:player")
    } else {
        Vec::new()
    };
    let is_html_player = og_values("video:type")
        .iter()
        .any(|t| t.eq_ignore_ascii_case("text/html"));
    if is_html_player {
        for key in ["video:secure_url", "video:url", "video"] {
            embed_candidates.extend(og_values(key));
        }
    }
    let embed_url = embed_candidates
        .into_iter()
        .filter_map(|u| normalize_embed_url(&u, page_url))
        .find(|u| plausible_embed_url(u, &image_urls));
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

/// Whether a normalized embed-player URL can plausibly be an HTML
/// player rather than a direct media file. Pages sometimes misuse
/// player tags for attachments; those URLs either duplicate the
/// card image or end in a recognizable media extension.
fn plausible_embed_url(embed_url: &str, image_urls: &HashSet<String>) -> bool {
    if image_urls.contains(embed_url) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(embed_url) else {
        return false;
    };
    let filename = parsed.path().rsplit('/').next().unwrap_or_default();
    let Some(ext) = filename.rsplit_once('.').map(|(_, ext)| ext.to_lowercase()) else {
        return true;
    };
    !matches!(
        ext.as_str(),
        "avif"
            | "bmp"
            | "flac"
            | "gif"
            | "heic"
            | "ico"
            | "jpeg"
            | "jpg"
            | "m3u8"
            | "m4a"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp3"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "oga"
            | "ogg"
            | "ogv"
            | "opus"
            | "png"
            | "svg"
            | "tif"
            | "tiff"
            | "wav"
            | "webm"
            | "webp"
    )
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
    let mut headers = HashMap::new();
    headers.insert(
        "User-Agent".to_string(),
        preview_user_agent(&ctx.config.federation.user_agent, &ctx.base_url()),
    );
    let response = crate::activitypub::actors::sync_call(move || http.get(&url, &headers)).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn excl() -> LinkExclusions {
        let mut e = LinkExclusions::default();
        e.urls.insert("remote.test/@alice".to_string());
        e.handles
            .insert(("alice".to_string(), "remote.test".to_string()));
        e.tags.insert("rust".to_string());
        e.tags.insert("応援".to_string());
        e
    }

    #[test]
    fn mention_url_and_handle_are_excluded() {
        let e = excl();
        // Stored profile URL — exact and scheme-agnostic.
        assert!(url_excluded("https://remote.test/@alice", &e));
        assert!(url_excluded("http://remote.test/@alice", &e));
        assert!(url_excluded("https://remote.test/@alice/", &e));
        // Same user on the mention's domain, different path shape.
        assert!(url_excluded("https://remote.test/users/alice", &e));
        assert!(url_excluded("https://remote.test/profile/alice", &e));
        // Other domains / users are not mentions.
        assert!(!url_excluded("https://other.test/@alice", &e));
        assert!(!url_excluded("https://remote.test/users/bob", &e));
    }

    #[test]
    fn tag_links_are_excluded() {
        let e = excl();
        for u in [
            "https://x.test/tags/rust",
            "https://x.test/tag/rust",
            "https://x.test/explore/tags/rust",
            "https://x.test/search?tag=rust",
            // Percent-encoded unicode tag name.
            "https://x.test/tags/%E5%BF%9C%E6%8F%B4",
        ] {
            assert!(url_excluded(u, &e), "{u}");
        }
        // Tag-shaped paths naming *other* tags stay eligible.
        assert!(!url_excluded("https://x.test/tags/python", &e));
        assert!(!url_excluded("https://blog.example/tags/post", &e));
    }

    #[test]
    fn plain_links_are_not_excluded() {
        let e = excl();
        assert!(!url_excluded("https://example.com/article", &e));
        assert!(!url_excluded("https://remote.test/blog/post", &e));
    }

    #[test]
    fn preview_ua_adds_mastodon_compat_token() {
        let ua = preview_user_agent("Yonder/0.1.0", "https://yonder.test");
        assert!(ua.starts_with("Yonder/0.1.0 (compatible; Mastodon/"));
        assert!(ua.contains("+https://yonder.test"));
        // Providers gate on a plain `Mastodon` substring.
        assert!(ua.contains("Mastodon"));
        // A UA already naming Mastodon is left alone.
        let custom = "Mastodon/4.2.0 (+https://x.test)";
        assert_eq!(preview_user_agent(custom, "https://x.test"), custom);
    }

    #[test]
    fn decode_segment_handles_percent_and_plus() {
        assert_eq!(decode_segment("%E5%BF%9C"), "応");
        assert_eq!(decode_segment("a+b"), "a+b");
        assert_eq!(decode_segment("100%25"), "100%");
        assert_eq!(decode_segment("%zz"), "%zz");
    }

    #[test]
    fn image_player_tags_do_not_create_video_embed() {
        // Akkoma emits one `twitter:player` per image attachment even
        // though the card is `summary_large_image`, not a player card.
        let img1 = "https://cdn.example/media/first.abc123";
        let img2 = "https://cdn.example/media/second.def456";
        let doc = format!(
            r#"<html><head>
                <meta property="og:title" content="Post with images">
                <meta property="og:image" content="{img1}">
                <meta property="og:image" content="{img2}">
                <meta name="twitter:card" content="summary_large_image">
                <meta name="twitter:player" content="{img1}">
                <meta name="twitter:card" content="summary_large_image">
                <meta name="twitter:player" content="{img2}">
            </head></html>"#
        );
        let meta = parse_metadata(&doc, "https://social.example/notice/1");
        assert_eq!(meta.embed_url, "");
        assert_eq!(meta.embed_width, 0);
        assert_eq!(meta.embed_height, 0);
        assert_eq!(meta.image.as_deref(), Some(img1));
    }

    #[test]
    fn declared_player_card_accepts_iframe_url() {
        let doc = r#"<html><head>
            <meta property="og:title" content="Watch this">
            <meta name="twitter:card" content="player">
            <meta name="twitter:player" content="https://player.example/embed/abc">
            <meta name="twitter:player:width" content="640">
            <meta name="twitter:player:height" content="360">
        </head></html>"#;
        let meta = parse_metadata(doc, "https://social.example/watch/1");
        assert_eq!(meta.embed_url, "https://player.example/embed/abc");
        assert_eq!(meta.embed_width, 640);
        assert_eq!(meta.embed_height, 360);
    }

    #[test]
    fn direct_media_player_url_is_rejected() {
        let doc = r#"<html><head>
            <meta name="twitter:card" content="player">
            <meta name="twitter:player" content="https://cdn.example/video.mp4">
        </head></html>"#;
        let meta = parse_metadata(doc, "https://social.example/watch/1");
        assert_eq!(meta.embed_url, "");
    }
}
