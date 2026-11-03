//! Remote outbox sync — the "sync posts from remote" profile action.
//!
//! Walks a remote actor's `outbox` ordered collection newest→oldest and
//! feeds each item through the same ingest path an inbox delivery would
//! take (`posts::remote::handle` for `Create`/`Update`/`Delete`, a
//! boost insert for `Announce`, a synthetic `Create` for bare objects —
//! the `materialize_status` pattern). Items already materialized are
//! skipped by a cheap `load_by_uri` pre-check, so gaps in the local
//! copy can sit anywhere in the collection and the scan never assumes
//! contiguity — it walks until `limit` *new* items land or a bound is
//! hit.
//!
//! Bounds: `min_id`/`max_id`-style windows resolve to [`SyncBounds`]
//! datetimes at the endpoint (the local status's `created_at` is the
//! remote `published` it was ingested with). Unbounded syncs instead
//! consult a Redis "floor" marker — the deepest `published` examined
//! last time — so a repeat click only walks items published since,
//! instead of re-scanning known territory. Explicit bounds bypass the
//! marker entirely: targeted gap repair.
//!
//! Remote politeness: every GET is the signed
//! [`lookup::fetch_remote_object_detailed`]; `429`/transient failures
//! retry with bounded exponential backoff, and the whole walk shares a
//! wall-clock deadline plus page/scan caps — a slow or throttling
//! remote degrades to a partial report, never a hung request.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use pubby_sdk::ActivityPubStorage;
use serde_json::{json, Value};

use crate::cache::RedisCache;
use crate::posts::{self, remote::NOTE_TYPES};
use crate::{Error, Result};

use super::lookup::{self, RemoteFetch};
use super::ApContext;

/// Default `limit` — activities imported per sync.
pub const DEFAULT_LIMIT: u32 = 25;
/// Hard ceiling on `limit` — a single sync never imports more.
pub const MAX_LIMIT: u32 = 100;

/// Outbox collection-page fetches per sync. Mastodon-sized pages
/// (~20 items) make this ~400 items of headroom over the scan cap.
const MAX_PAGES: u32 = 20;
/// Whole-walk wall clock — the endpoint answers within it; what was
/// imported stays imported (partial report).
const DEADLINE: Duration = Duration::from_secs(90);
/// Per-request retry delays for 429/transient failures — three extra
/// attempts at most, each clipped to the remaining deadline.
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];
/// Redis marker TTL for the unbounded-scan floor and the in-flight
/// lock. A lost floor just costs extra pages on the next sync.
const MARKER_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// In-flight lock lease — the deadline plus margin; released on
/// completion so a successful sync doesn't throttle the next.
const LOCK_TTL_SECONDS: usize = (DEADLINE.as_secs() + 60) as usize;

/// Resolved datetime window for a sync (endpoint turns `min_id`/
/// `max_id`/`min_date`/`max_date` into these).
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncBounds {
    /// Import only items published strictly after this.
    pub since: Option<DateTime<Utc>>,
    /// Import only items published strictly before this.
    pub until: Option<DateTime<Utc>>,
}

/// What the endpoint reports back.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Items that changed the local database (new statuses, applied
    /// edits, deletes, boosts).
    pub imported: u32,
    /// Outbox items examined (in-window or not).
    pub scanned: u32,
    /// Collection pages fetched (excludes the root document).
    pub pages: u32,
    /// `true` when the scan reached the window's end — collection
    /// exhausted, `since`/floor met — so nothing more remains in this
    /// window. `false` means a cap or the deadline stopped it early.
    pub complete: bool,
}

/// `sync_account_outbox` for `account_id`'s remote actor: resolve the
/// actor's outbox, then walk it newest→oldest importing up to `limit`
/// not-yet-materialized items inside `bounds`.
///
/// Fails fast on a held in-flight lock ([`Error::RateLimited`]), a
/// missing/gone outbox, or the actor doc being unreachable; later
/// page/item failures degrade to a partial report instead.
pub async fn sync_account_outbox(
    ap: &ApContext,
    account_id: i64,
    actor_id: &str,
    limit: u32,
    bounds: &SyncBounds,
) -> Result<SyncReport> {
    if !claim_sync_lock(ap, account_id).await {
        return Err(Error::RateLimited(
            "a remote sync is already running for this account".to_string(),
        ));
    }
    let report = run(ap, account_id, actor_id, limit, bounds).await;
    release_sync_lock(ap, account_id).await;
    report
}

/// `SET … NX EX` on the per-account lock — one sync walks an outbox at
/// a time; a second caller 429s instead of doubling the remote's load.
async fn claim_sync_lock(ap: &ApContext, account_id: i64) -> bool {
    let key = ap
        .core
        .redis
        .key(&format!("remote-outbox-sync:{account_id}"));
    let mut conn = match ap.core.redis.conn().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "redis unavailable — refusing outbox sync");
            return false;
        }
    };
    match redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(LOCK_TTL_SECONDS)
        .query_async::<Option<String>>(&mut *conn)
        .await
    {
        Ok(claimed) => claimed.is_some(),
        Err(e) => {
            tracing::warn!(account_id, error = %e, "outbox sync lock check failed");
            false
        }
    }
}

/// Release the lock — best-effort; the lease lapses on its own.
async fn release_sync_lock(ap: &ApContext, account_id: i64) {
    let key = ap
        .core
        .redis
        .key(&format!("remote-outbox-sync:{account_id}"));
    let Ok(mut conn) = ap.core.redis.conn().await else {
        return;
    };
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&key).query_async(&mut *conn).await;
}

/// A signed remote GET with bounded retry: `429`/`Failed` back off
/// through [`RETRY_BACKOFF`] (clipped to the deadline), everything
/// else returns as-is. `pub(crate)` — the thread-context fetch shares
/// the same remote politeness budget.
pub(crate) async fn fetch_doc(ap: &ApContext, url: &str, deadline: Instant) -> Result<RemoteFetch> {
    let mut attempt = 0usize;
    loop {
        match lookup::fetch_remote_object_detailed(ap, url).await? {
            RemoteFetch::RateLimited | RemoteFetch::Failed if attempt < RETRY_BACKOFF.len() => {
                let wait =
                    RETRY_BACKOFF[attempt].min(deadline.saturating_duration_since(Instant::now()));
                if wait.is_zero() {
                    return Ok(RemoteFetch::Failed);
                }
                attempt += 1;
                tracing::debug!(url = %url, wait_ms = wait.as_millis() as u64, "remote fetch retry backoff");
                tokio::time::sleep(wait).await;
            }
            outcome => return Ok(outcome),
        }
    }
}

/// The actor's outbox URL: read from `actor_cache` (any age — the
/// outbox URL is stable), else signed-fetch the actor document, cache
/// + upsert it (a free profile refresh) and extract `outbox`.
async fn resolve_outbox_url(
    ap: &ApContext,
    actor_id: &str,
    deadline: Instant,
) -> Result<Option<String>> {
    let cached: Option<Value> =
        sqlx::query_scalar("SELECT actor_data FROM actor_cache WHERE actor_id = $1")
            .bind(actor_id)
            .fetch_optional(&ap.core.db)
            .await?;
    if let Some(url) = cached
        .as_ref()
        .and_then(|doc| lookup::actor_collection_uri(doc, "outbox"))
    {
        return Ok(Some(url));
    }
    match fetch_doc(ap, actor_id, deadline).await? {
        RemoteFetch::Doc(fetched) => {
            if let Err(e) = ap
                .core
                .storage
                .cache_remote_actor(actor_id, &fetched.doc, None)
            {
                tracing::warn!(actor_id, error = %e, "actor cache write failed");
            }
            if let Err(e) =
                super::storage::upsert_remote_actor_row(&ap.core.db, actor_id, &fetched.doc).await
            {
                tracing::warn!(actor_id, error = %e, "actor row upsert failed");
            }
            Ok(lookup::actor_collection_uri(&fetched.doc, "outbox"))
        }
        RemoteFetch::Gone => Err(Error::NotFound("remote actor".to_string())),
        RemoteFetch::RateLimited | RemoteFetch::Failed => Err(Error::Internal(format!(
            "remote actor fetch failed: {actor_id}"
        ))),
    }
}

/// Where the walk continues from after `doc`: an inline `first` page,
/// a `first`/`next` link, or nothing (collection exhausted).
/// `pub(crate)` — `replies` collections paginate the same way.
pub(crate) enum PageRef {
    Inline(Value),
    Url(String),
}

/// `first`/`next` link: bare URL string, `{id}`/`{href}` reference —
/// or, for `first`, an inline page object (`orderedItems` inside).
/// `pub(crate)` — the thread-context fetch paginates `replies`
/// collections with it.
pub(crate) fn link_ref(doc: &Value, key: &str) -> Option<PageRef> {
    match doc.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(PageRef::Url(s.clone())),
        Some(Value::Object(o)) => {
            if o.get("orderedItems").is_some() || o.get("items").is_some() {
                return Some(PageRef::Inline(doc.get(key).cloned().unwrap_or_default()));
            }
            o.get("id")
                .or_else(|| o.get("href"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| PageRef::Url(s.to_string()))
        }
        _ => None,
    }
}

/// An item's ordering key: the activity's `published`, else the
/// embedded object's (some servers stamp only the object).
fn item_published(item: &Value) -> Option<DateTime<Utc>> {
    item.get("published")
        .or_else(|| item.get("object").and_then(|o| o.get("published")))
        .and_then(pubby_sdk::model::parse_dt)
}

/// The Redis floor marker: deepest `published` an *unbounded* sync
/// reached. Stored per account; a lost marker is just a deeper scan.
async fn read_floor(redis: &crate::cache::Redis, account_id: i64) -> Option<DateTime<Utc>> {
    let cache = RedisCache::new(redis.clone());
    let marker = cache
        .get(&format!("remote-outbox-sync:{account_id}"))
        .await
        .ok()
        .flatten()?;
    let raw = marker.get("floor").and_then(|v| v.as_str())?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Persist `floor` as the new unbounded-scan frontier — unbounded
/// scans only (a bounded gap repair must not move it).
async fn write_floor(redis: &crate::cache::Redis, account_id: i64, floor: DateTime<Utc>) {
    let cache = RedisCache::new(redis.clone());
    if let Err(e) = cache
        .set(
            &format!("remote-outbox-sync:{account_id}"),
            &json!({ "floor": floor.to_rfc3339() }),
            MARKER_TTL,
        )
        .await
    {
        tracing::warn!(account_id, error = %e, "outbox floor marker write failed");
    }
}

async fn run(
    ap: &ApContext,
    account_id: i64,
    actor_id: &str,
    limit: u32,
    bounds: &SyncBounds,
) -> Result<SyncReport> {
    let deadline = Instant::now() + DEADLINE;
    let unbounded = bounds.since.is_none() && bounds.until.is_none();
    let floor = if unbounded {
        read_floor(&ap.core.redis, account_id).await
    } else {
        None
    };
    // Items examined beyond `limit` still find gaps — already-known
    // items only consume the scan budget, so a mostly-synced profile
    // keeps digging instead of reporting a false zero.
    let scan_cap = (limit as u64 * 4).clamp(50, 500);

    let Some(outbox_url) = resolve_outbox_url(ap, actor_id, deadline).await? else {
        return Err(Error::Validation(
            "remote actor advertises no outbox".to_string(),
        ));
    };
    let root = match fetch_doc(ap, &outbox_url, deadline).await? {
        RemoteFetch::Doc(fetched) => fetched.doc,
        RemoteFetch::Gone => return Err(Error::NotFound("remote outbox".to_string())),
        RemoteFetch::RateLimited | RemoteFetch::Failed => {
            return Err(Error::Internal(format!(
                "remote outbox unreachable: {outbox_url}"
            )))
        }
    };

    let mut report = SyncReport::default();
    let mut next = link_ref(&root, "first");
    // Deepest `published` examined — becomes the new floor marker.
    let mut frontier: Option<DateTime<Utc>> = None;
    let mut exhausted = false;

    'pages: while let Some(page) = next.take() {
        let doc = match page {
            PageRef::Inline(doc) => doc,
            PageRef::Url(url) => {
                if report.pages >= MAX_PAGES || Instant::now() >= deadline {
                    break;
                }
                match fetch_doc(ap, &url, deadline).await? {
                    RemoteFetch::Doc(fetched) => {
                        report.pages += 1;
                        fetched.doc
                    }
                    // A dead page keeps what we have — partial report.
                    RemoteFetch::Gone | RemoteFetch::RateLimited | RemoteFetch::Failed => {
                        break 'pages
                    }
                }
            }
        };
        let Some(items) = doc
            .get("orderedItems")
            .or_else(|| doc.get("items"))
            .and_then(Value::as_array)
        else {
            break 'pages;
        };
        for item in items {
            if Instant::now() >= deadline
                || report.imported >= limit
                || report.scanned as u64 >= scan_cap
            {
                break 'pages;
            }
            report.scanned += 1;
            let published = item_published(item);
            // Too new for the window — keep scanning (items only get
            // older from here, but the bound is per-item for safety).
            if published.is_some_and(|p| bounds.until.is_some_and(|u| p >= u)) {
                continue;
            }
            // Reached the window's lower bound or the previous sync's
            // frontier — everything below was already offered.
            if published.is_some_and(|p| bounds.since.is_some_and(|s| p <= s))
                || published.is_some_and(|p| floor.is_some_and(|f| p <= f))
            {
                exhausted = true;
                break 'pages;
            }
            if let Some(p) = published {
                frontier = Some(frontier.map_or(p, |f: DateTime<Utc>| f.min(p)));
            }
            match ingest_item(ap, account_id, actor_id, item, deadline).await {
                Ok(true) => report.imported += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(account_id, error = %e, "outbox item ingest failed")
                }
            }
        }
        next = link_ref(&doc, "next");
        exhausted = next.is_none();
    }
    report.complete = exhausted;

    if unbounded {
        if let Some(f) = frontier {
            write_floor(&ap.core.redis, account_id, f).await;
        }
    }
    Ok(report)
}

/// Feed one outbox item through the ingest path. Returns `true` when
/// the local database changed — that's the `imported` counter.
async fn ingest_item(
    ap: &ApContext,
    account_id: i64,
    actor_id: &str,
    item: &Value,
    deadline: Instant,
) -> Result<bool> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "Create" | "Update" | "Delete" => {
            let activity = pubby_sdk::Activity::from_json(item);
            // An outbox may only speak for its owner — anything else
            // is for normal federation to deliver.
            if activity.actor != actor_id {
                return Ok(false);
            }
            if item_type == "Delete" {
                let Some(uri) = crate::interactions::object_uri(item.get("object")) else {
                    return Ok(false);
                };
                let live = posts::load_by_uri(&ap.core.db, &uri)
                    .await?
                    .is_some_and(|s| !s.tombstone);
                if !live {
                    return Ok(false);
                }
                posts::remote::handle_as(
                    &ap.core,
                    &activity,
                    item,
                    posts::remote::IngestOrigin::Fetch,
                )
                .await;
                return Ok(posts::load_by_uri(&ap.core.db, &uri)
                    .await?
                    .is_some_and(|s| s.tombstone));
            }
            let Some(object_id) = crate::interactions::object_uri(item.get("object")) else {
                return Ok(false);
            };
            // Never resurrect a tombstoned row — our Delete record is
            // authoritative over a stale outbox entry.
            let existing = posts::load_by_uri(&ap.core.db, &object_id).await?;
            if existing.as_ref().is_some_and(|s| s.tombstone) {
                return Ok(false);
            }
            posts::remote::handle_as(
                &ap.core,
                &activity,
                item,
                posts::remote::IngestOrigin::Fetch,
            )
            .await;
            Ok(existing.is_none())
        }
        "Announce" => ingest_announce(ap, account_id, actor_id, item, deadline).await,
        // Bare objects — some outboxes list the posts themselves.
        _ if NOTE_TYPES.contains(&item_type) => {
            let Some(author) = crate::search::resolve::attributed_to(item) else {
                return Ok(false);
            };
            if author != actor_id {
                return Ok(false);
            }
            let Some(object_id) = item.get("id").and_then(|v| v.as_str()) else {
                return Ok(false);
            };
            let existing = posts::load_by_uri(&ap.core.db, object_id).await?;
            if existing.as_ref().is_some_and(|s| s.tombstone) {
                return Ok(false);
            }
            // Same synthetic-Create wrap as `materialize_status`.
            let raw = json!({
                "id": format!("{object_id}#activity"),
                "type": "Create",
                "actor": author,
                "to": item.get("to").cloned().unwrap_or(Value::Null),
                "cc": item.get("cc").cloned().unwrap_or(Value::Null),
                "published": item.get("published").cloned().unwrap_or(Value::Null),
                "object": item,
            });
            let activity = pubby_sdk::Activity::from_json(&raw);
            posts::remote::handle_as(
                &ap.core,
                &activity,
                &raw,
                posts::remote::IngestOrigin::Fetch,
            )
            .await;
            Ok(existing.is_none())
        }
        _ => Ok(false),
    }
}

/// `Announce` backfill — `interactions::inbound`'s gate only touches
/// *local* statuses (remote-to-remote isn't its business), so the
/// boost row is written here with the same insert. The announced
/// object is fetched when it isn't materialized yet — that costs a
/// remote GET inside the same deadline/retry budget as the pages.
async fn ingest_announce(
    ap: &ApContext,
    account_id: i64,
    actor_id: &str,
    item: &Value,
    deadline: Instant,
) -> Result<bool> {
    let activity = pubby_sdk::Activity::from_json(item);
    if activity.actor != actor_id {
        return Ok(false);
    }
    let Some(uri) = crate::interactions::object_uri(item.get("object")) else {
        return Ok(false);
    };
    let mut target = posts::load_by_uri(&ap.core.db, &uri)
        .await?
        .filter(|s| !s.tombstone);
    if target.is_none() && Instant::now() < deadline {
        if let RemoteFetch::Doc(fetched) = fetch_doc(ap, &uri, deadline).await? {
            let _ = crate::search::resolve::materialize_remote(
                &ap.core.db,
                ap,
                &fetched.url,
                &fetched.doc,
            )
            .await;
            target = posts::load_by_uri(&ap.core.db, &uri)
                .await?
                .filter(|s| !s.tombstone);
        }
    }
    let Some(target) = target else {
        return Ok(false);
    };
    // Reblog-of-reblog normalizes to the original (announce parity).
    let target = match target.reblog_of_id {
        Some(orig) => posts::load(&ap.core.db, orig)
            .await?
            .filter(|s| !s.tombstone)
            .unwrap_or(target),
        None => target,
    };
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE account_id = $1 AND NOT tombstone \
         AND (reblog_of_id = $2 OR uri = $3))",
    )
    .bind(account_id)
    .bind(target.id)
    .bind(&activity.id)
    .fetch_one(&ap.core.db)
    .await?;
    if already {
        return Ok(false);
    }
    let mut tx = ap.core.db.begin().await?;
    let boost_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, content, content_html, content_type, visibility, \
             local, reblog_of_id, uri, url, created_at) \
         VALUES (timestamp_id('statuses', LEAST(COALESCE($5, now()), now())), \
                 $1,'','','text/plain','public',false,$2,$3,$4, \
                 LEAST(COALESCE($5, now()), now())) \
         ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL AND NOT tombstone \
         DO NOTHING RETURNING id",
    )
    .bind(account_id)
    .bind(target.id)
    .bind(if activity.id.is_empty() {
        None
    } else {
        Some(activity.id.clone())
    })
    .bind(&uri)
    .bind(activity.published)
    .fetch_optional(&mut *tx)
    .await?;
    if boost_id.is_some() {
        sqlx::query("UPDATE statuses SET reblogs_count = reblogs_count + 1 WHERE id = $1")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let Some(boost_id) = boost_id else {
        return Ok(false);
    };
    crate::timelines::fanout::enqueue(&ap.core.dispatcher, boost_id).await;
    if let Ok(Some(boost)) = posts::load(&ap.core.db, boost_id).await {
        let ctx = crate::timelines::stream::post_ctx_for_core(&ap.core);
        crate::timelines::stream::publish_update(&ctx, &boost).await;
    }
    Ok(true)
}
