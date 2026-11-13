//! Proxied liker/booster lists for remote statuses.
//!
//! `favourited_by`/`reblogged_by` read local rows; for remote statuses
//! whose origin reports more likes/shares than we received, the
//! remainder is proxied page-by-page from the origin's collection —
//! items map to account stubs via `ensure_account`, fetched pages are
//! cached in Redis (`liker_page_cache_ttl_seconds`), and per-like
//! rows are never written.
//!
//! Pagination: local rows come first; when the local page under-fills
//! (or `max_id` is already a remote cursor) origin pages fill the
//! remainder, across pages until the response limit or the fetch cap.
//! Remote cursors are negative — a value of `-K` means "K remote
//! items already served"; `liker-cursor:{status}:{kind}:{K}` maps it
//! to a resume position `{"page": …, "offset": …}` inside the origin
//! collection, so mid-page stops lose nothing. An expired cursor
//! degrades to an empty page, never an error.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::cache::{Redis, RedisCache};
use crate::http::paginate::Page;
use crate::jobs::Dispatcher;
use crate::posts::StatusRow;
use crate::Result;

use super::lookup::RemoteFetch;
use super::outbox::{self, PageRef};
use super::ApContext;

/// `likes` (favourited_by) or `shares` (reblogged_by).
#[derive(Clone, Copy)]
pub(crate) enum ListKind {
    Likes,
    Shares,
}

impl ListKind {
    /// The document field carrying the collection.
    fn collection_key(self) -> &'static str {
        match self {
            ListKind::Likes => "likes",
            ListKind::Shares => "shares",
        }
    }

    /// The remote total recorded at expansion.
    fn remote_total(self, status: &StatusRow) -> i64 {
        match self {
            ListKind::Likes => status.remote_likes_count,
            ListKind::Shares => status.remote_shares_count,
        }
        .unwrap_or(0) as i64
    }

    /// The local transactional counter the remote total is compared
    /// against — the proxy only fills the gap between them.
    fn local_total(self, status: &StatusRow) -> i64 {
        (match self {
            ListKind::Likes => status.favourites_count,
            ListKind::Shares => status.reblogs_count,
        }) as i64
    }

    /// Every local interactor for the status — the remote fill dedupes
    /// against this (the origin's collection counts our own likes
    /// too, not just the current page's).
    async fn local_ids(self, db: &PgPool, status_id: i64) -> Result<HashSet<i64>> {
        let sql = match self {
            ListKind::Likes => "SELECT account_id FROM favourites WHERE status_id = $1",
            ListKind::Shares => {
                "SELECT account_id FROM statuses WHERE reblog_of_id = $1 AND NOT tombstone"
            }
        };
        let ids: Vec<i64> = sqlx::query_scalar(sql)
            .bind(status_id)
            .fetch_all(db)
            .await?;
        Ok(ids.into_iter().collect())
    }

    /// Redis key segment.
    fn tag(self) -> &'static str {
        self.collection_key()
    }
}

/// Bound for a single remote page GET — short enough to keep the API
/// request responsive.
const PAGE_DEADLINE: Duration = Duration::from_secs(10);
/// Origin pages resolved per request — each may be a remote GET on a
/// cache miss, so the fill can't chase a deep collection in one call.
const MAX_PAGE_RESOLUTIONS: u32 = 3;
/// Cursor pseudo-pages: the collection's first page (resolved from
/// the status doc) and the end of the list.
const FIRST_PAGE: &str = "@first";
const END_PAGE: &str = "@end";

/// A resume position inside the origin collection — `page` is an
/// origin page URL, [`FIRST_PAGE`] (the collection resolved from the
/// status doc) or [`END_PAGE`]; `offset` skips items already served
/// inside that page.
#[derive(Clone, Serialize, Deserialize)]
struct Cursor {
    page: String,
    offset: usize,
}

/// Append origin-collection accounts to `local` rows — the remote
/// fill for `favourited_by`/`reblogged_by`. Empty when the status
/// isn't remote, the remote total doesn't exceed the local counter,
/// the local page already filled, or the remote page is unreachable
/// (degradation is silent by design: callers serve what they have).
/// `viewer` applies the same block/domain-block/suspended filters as
/// the local page before an item can consume response room.
pub(crate) async fn fill(
    ctx: &crate::posts::PostContext,
    ap: &ApContext,
    status: &StatusRow,
    kind: ListKind,
    page: &Page,
    local: &[(i64, i64)],
    viewer: Option<i64>,
) -> Result<Vec<(i64, i64)>> {
    let db = &ctx.db;
    let redis = &ctx.redis;
    let dispatcher = &ctx.dispatcher;
    if status.local || status.uri.is_none() || kind.remote_total(status) <= kind.local_total(status)
    {
        return Ok(Vec::new());
    }
    // Remote cursors are negative — `page.max_id < 0` means "continue
    // the remote list" and the local page was exhausted (it returns
    // nothing anyway since ids are positive). A full local page means
    // locals come first; the remote fill kicks in on the next page.
    let remote_cursor = page.max_id.unwrap_or(0) < 0;
    if !remote_cursor && local.len() as i64 >= page.limit {
        return Ok(Vec::new());
    }
    let prev_offset = page.max_id.map(|m| -m).unwrap_or(0).max(0);
    let room = (page.limit - local.len() as i64).max(1);
    let ttl = ap.core.config.remote_context.liker_page_cache_ttl_seconds;

    // The resume position: the stored cursor for a continuing page,
    // or the collection's first page for a fresh request.
    let mut position = if remote_cursor {
        match cursor_read(redis, status.id, kind, prev_offset).await {
            Some(c) => Some(c),
            None => return Ok(Vec::new()), // expired cursor — empty page
        }
    } else {
        Some(Cursor {
            page: FIRST_PAGE.to_string(),
            offset: 0,
        })
    };

    // Map item → actor URI → account stub, deduped against *all*
    // local interactors (not just this page) and within itself.
    let mut seen = kind.local_ids(db, status.id).await?;
    seen.extend(local.iter().map(|(a, _)| *a));
    let mut extra: Vec<(i64, i64)> = Vec::new();
    let mut resolutions = 0u32;
    while let Some(pos) = position.clone() {
        if extra.len() as i64 >= room || resolutions >= MAX_PAGE_RESOLUTIONS {
            break;
        }
        resolutions += 1;
        let Some(items) = resolve_page(ap, redis, status, kind, &pos, ttl).await? else {
            position = None;
            break;
        };
        // Serve items from `pos.offset`, advancing the cursor past
        // everything examined — valid or not.
        let mut idx = pos.offset;
        while idx < items.uris.len() && (extra.len() as i64) < room {
            let uri = &items.uris[idx];
            idx += 1;
            if !valid_actor_uri(uri) {
                continue;
            }
            let Ok(account_id) = ensure_actor_account(dispatcher, db, &ap.core.config, uri).await
            else {
                continue;
            };
            let visible = crate::social::visibility::visible(db, account_id).await?;
            let hidden = match viewer {
                Some(v) => crate::social::visibility::author_hidden(db, v, account_id).await?,
                None => false,
            };
            if !visible || hidden {
                seen.insert(account_id);
                continue;
            }
            if seen.insert(account_id) {
                let cursor = -(prev_offset + extra.len() as i64 + 1);
                extra.push((account_id, cursor));
            }
        }
        position = if idx < items.uris.len() {
            Some(Cursor {
                page: pos.page,
                offset: idx,
            })
        } else {
            items.next.map(|n| Cursor { page: n, offset: 0 })
        };
    }
    // The last emitted row's cursor keys the resume position in
    // Redis — `page.max_id` round-trips it. Written even at the end
    // of the list so a full page's `next` link resolves (to an
    // empty fill).
    if let Some((_, last_cursor)) = extra.last() {
        let at = position.unwrap_or(Cursor {
            page: END_PAGE.to_string(),
            offset: 0,
        });
        cursor_write(redis, status.id, kind, -last_cursor, &at, ttl).await;
    }
    Ok(extra)
}

/// `ensure_account` over a `Dispatcher` + `PgPool` — the
/// `posts::remote` helper takes `ApCore`, which the API layer doesn't
/// have; the job enqueue and upsert are all it does.
async fn ensure_actor_account(
    dispatcher: &Dispatcher,
    db: &PgPool,
    config: &crate::config::Config,
    actor_url: &str,
) -> Result<i64> {
    super::lookup::upsert_remote_stub(db, dispatcher, config, actor_url).await
}

/// A cheap sanity check before an item URI is turned into an account
/// stub — only HTTP(S) URIs become actors.
fn valid_actor_uri(uri: &str) -> bool {
    url::Url::parse(uri)
        .map(|u| matches!(u.scheme(), "http" | "https"))
        .unwrap_or(false)
}

/// An origin collection page: actor URIs plus the next page URL.
#[derive(Serialize, Deserialize)]
struct PageItems {
    uris: Vec<String>,
    next: Option<String>,
}

/// `liker-cursor:{status_id}:{kind}:{offset}` — resume position for
/// a remote cursor.
fn cursor_key(status_id: i64, kind: ListKind, offset: i64) -> String {
    format!("liker-cursor:{}:{}:{}", status_id, kind.tag(), offset)
}

/// `liker-items:{status_id}:{kind}:{sha256(url)[..32]}` — cached page
/// contents for one origin page URL.
fn items_key(status_id: i64, kind: ListKind, url: &str) -> String {
    format!(
        "liker-items:{}:{}:{}",
        status_id,
        kind.tag(),
        &hex::encode(Sha256::digest(url.as_bytes()))[..32]
    )
}

/// `liker-coll:{status_id}:{kind}` — cached first-page resolution of
/// the status's collection (the status doc isn't re-fetched per
/// request).
fn coll_key(status_id: i64, kind: ListKind) -> String {
    format!("liker-coll:{}:{}", status_id, kind.tag())
}

async fn cursor_read(redis: &Redis, status_id: i64, kind: ListKind, offset: i64) -> Option<Cursor> {
    RedisCache::new(redis.clone())
        .get(&redis.key(&cursor_key(status_id, kind, offset)))
        .await
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value::<Cursor>(v).ok())
}

async fn cursor_write(
    redis: &Redis,
    status_id: i64,
    kind: ListKind,
    offset: i64,
    cursor: &Cursor,
    ttl_seconds: u64,
) {
    let _ = RedisCache::new(redis.clone())
        .set(
            &redis.key(&cursor_key(status_id, kind, offset)),
            &json!(cursor),
            Duration::from_secs(ttl_seconds),
        )
        .await;
}

/// Resolve a cursor position to page contents — `@first` through the
/// (cached) collection resolution, a URL through the (cached) page
/// fetch, `@end` to nothing.
async fn resolve_page(
    ap: &ApContext,
    redis: &Redis,
    status: &StatusRow,
    kind: ListKind,
    cursor: &Cursor,
    ttl_seconds: u64,
) -> Result<Option<PageItems>> {
    match cursor.page.as_str() {
        END_PAGE => Ok(None),
        FIRST_PAGE => collection_first_page(ap, redis, status, kind, ttl_seconds).await,
        url => fetch_page_items(ap, redis, status.id, kind, url, ttl_seconds).await,
    }
}

/// Extract `orderedItems`/`items` as actor URIs plus the `next` link.
fn page_items(doc: &Value) -> PageItems {
    let items = doc
        .get("orderedItems")
        .or_else(|| doc.get("items"))
        .and_then(Value::as_array);
    let uris = items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            // A string is already the URI (actor id on Mastodon-flavor
            // collections); objects carry `actor`/`attributedTo`/`id`.
            if let Some(s) = item.as_str() {
                return Some(s.to_string());
            }
            for key in ["actor", "attributedTo", "id"] {
                if let Some(v) = item.get(key).and_then(Value::as_str) {
                    return Some(v.to_string());
                }
            }
            None
        })
        .collect();
    let next = match outbox::link_ref(doc, "next") {
        Some(PageRef::Url(u)) => Some(u),
        Some(PageRef::Inline(d)) => d.get("id").and_then(Value::as_str).map(str::to_string),
        None => None,
    };
    PageItems { uris, next }
}

/// Fetch one collection page — Redis-cached contents (`uris` + the
/// `next` link, so cache hits keep paginating), a bounded `fetch_doc`
/// on miss. `None` = dead page (callers degrade to an empty fill).
async fn fetch_page_items(
    ap: &ApContext,
    redis: &Redis,
    status_id: i64,
    kind: ListKind,
    url: &str,
    ttl_seconds: u64,
) -> Result<Option<PageItems>> {
    let cache = RedisCache::new(redis.clone());
    let key = redis.key(&items_key(status_id, kind, url));
    if let Ok(Some(cached)) = cache.get(&key).await {
        if let Ok(items) = serde_json::from_value::<PageItems>(cached) {
            return Ok(Some(items));
        }
    }
    let doc = match outbox::fetch_doc(ap, url, Instant::now() + PAGE_DEADLINE).await? {
        RemoteFetch::Doc(f) => f.doc,
        RemoteFetch::Gone | RemoteFetch::RateLimited | RemoteFetch::Failed => return Ok(None),
    };
    // A fetched document may be the collection *root* rather than a
    // page — descend into `first` once.
    let doc = if doc.get("orderedItems").is_none() && doc.get("items").is_none() {
        match outbox::link_ref(&doc, "first") {
            Some(PageRef::Url(u)) => {
                match outbox::fetch_doc(ap, &u, Instant::now() + PAGE_DEADLINE).await? {
                    RemoteFetch::Doc(f) => f.doc,
                    _ => return Ok(None),
                }
            }
            Some(PageRef::Inline(d)) => d,
            None => return Ok(None),
        }
    } else {
        doc
    };
    let items = page_items(&doc);
    let _ = cache
        .set(&key, &json!(items), Duration::from_secs(ttl_seconds))
        .await;
    Ok(Some(items))
}

/// The first page of the status's `likes`/`shares` collection —
/// resolved once per TTL window and cached under `liker-coll`, so
/// repeat requests don't re-fetch the status document. The status
/// doc carries the collection ref (inline items, a `first` link, or
/// a bare collection URL).
async fn collection_first_page(
    ap: &ApContext,
    redis: &Redis,
    status: &StatusRow,
    kind: ListKind,
    ttl_seconds: u64,
) -> Result<Option<PageItems>> {
    let cache = RedisCache::new(redis.clone());
    let key = redis.key(&coll_key(status.id, kind));
    if let Ok(Some(cached)) = cache.get(&key).await {
        if let Ok(items) = serde_json::from_value::<PageItems>(cached) {
            return Ok(Some(items));
        }
    }
    let Some(status_url) = status.uri.clone() else {
        return Ok(None);
    };
    let doc = match outbox::fetch_doc(ap, &status_url, Instant::now() + PAGE_DEADLINE).await? {
        RemoteFetch::Doc(f) => f.doc,
        _ => return Ok(None),
    };
    let Some(coll) = doc.get(kind.collection_key()) else {
        return Ok(None);
    };
    // Inline items need no further fetch; a `first` link or a bare
    // collection URL resolves through `fetch_page_items`.
    let items = if coll.get("orderedItems").is_some() || coll.get("items").is_some() {
        Some(page_items(coll))
    } else {
        let url = match coll {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(_) => outbox::link_ref(coll, "first").and_then(|r| match r {
                PageRef::Url(u) => Some(u),
                PageRef::Inline(_) => None,
            }),
            _ => None,
        };
        match url {
            Some(u) => fetch_page_items(ap, redis, status.id, kind, &u, ttl_seconds).await?,
            // `first` was an inline page object.
            None => match outbox::link_ref(coll, "first") {
                Some(PageRef::Inline(d)) => Some(page_items(&d)),
                _ => None,
            },
        }
    };
    if let Some(items) = &items {
        let _ = cache
            .set(&key, &json!(items), Duration::from_secs(ttl_seconds))
            .await;
    }
    Ok(items)
}
