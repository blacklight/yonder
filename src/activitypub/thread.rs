//! Remote thread-context fetch — shared expansion primitives plus the
//! lazy "expand thread" behind
//! `POST /api/v1/statuses/{id}/fetch_remote_context` and the
//! selection-time expansion behind `GET /api/v1/statuses/{id}/context`.
//!
//! A remote reply often arrives with a broken local chain: its
//! `inReplyTo` was never materialized (the URI isn't stored locally),
//! so `posts::context` sees no ancestors and the user only gets the
//! one post. Two triggers expand the thread — the explicit endpoint
//! (the SPA's "expand" action) and a plain `GET /context` when the
//! status's expansion grace expired — both walking the *remote* thread
//! and materializing it through the same ingest path an inbox delivery
//! would take (`search::resolve::materialize_remote` — per-origin
//! trust checks included):
//!
//! * **Ancestors** — re-fetch the status's own object, then climb
//!   `inReplyTo`. A locally-known parent whose `in_reply_to_id` is
//!   still `NULL` means the gap continues above it — its remote doc
//!   is fetched to keep climbing (no re-ingest needed). Ancestor docs
//!   are ingested root-first so every child finds its parent locally;
//!   the target is re-ingested last, which repairs the `NULL`
//!   `in_reply_to_id` it was stored with (the `ON CONFLICT (uri)`
//!   upsert rewrites the link). The `ThreadPath` job climbs from a
//!   stored row's `in_reply_to_uri` without fetching the target doc.
//! * **Replies** — breadth-first expansion of the target's and each
//!   fetched ancestor's `replies` collection (inline items or linked
//!   pages). Collection items are materialized by URI: an inline
//!   object that passes the same-origin trust check for the page it
//!   arrived in ingests directly; anything else is fetched from its
//!   own origin first — replies by other authors are the point of the
//!   collection, so no ownership check applies (unlike outbox sync).
//!
//! Bounds: every remote GET goes through [`outbox::fetch_doc`]
//! (signed, 429/transient → bounded backoff); every walk shares a
//! wall-clock deadline, a per-status Redis lock and caps on chain
//! length, collection pages, remote GETs, expansion depth and per-
//! collection breadth — a hostile or huge thread degrades to a
//! partial report, never a hung request. Anything not reached stays
//! lazy: opening that status expands its own context.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::config::RemoteContextConfig;
use crate::jobs::{JobContext, JobHandler, JobKind};
use crate::posts::{self, StatusRow};
use crate::search::resolve::ResolvedHit;
use crate::{Error, Result};

use super::lookup::{self, RemoteFetch};
use super::outbox::{self, PageRef};
use super::ApContext;

/// Whole-walk wall clock for `fetch_remote_context` — the endpoint
/// answers within it; what was materialized stays materialized
/// (partial report).
const DEADLINE: Duration = Duration::from_secs(60);
/// Ancestor-climb cap for the full walk — matches `posts::context`'s
/// walk depth, so a corrupted `inReplyTo` cycle can't spin the fetch.
const MAX_CHAIN: u32 = 64;
/// Replies-of-replies expansion depth for the full walk — deeper
/// nesting stays lazy (opening that status expands its own context).
const MAX_DEPTH: u32 = 10;
/// Remote object GETs per full fetch — the target doc, ancestor docs
/// and replies materialized from their own origin.
const MAX_FETCHES: u32 = 100;
/// `replies` collection pages per full fetch.
const MAX_PAGES: u32 = 20;
/// `ThreadPath` job deadline — an ancestor-only climb should finish
/// well inside the job's retry window.
const PATH_DEADLINE: Duration = Duration::from_secs(15);
/// In-flight lock lease — the deadline plus margin; released on
/// completion so a successful fetch doesn't throttle the next.
const LOCK_TTL_SECONDS: usize = (DEADLINE.as_secs() + 60) as usize;

/// Fetch/ingest budget plus walk counters — one `Walk` carries an
/// expansion's remaining capacity and its running report. `full()`
/// preserves `fetch_remote_context`'s historical caps; `quick()`
/// bounds an inline `GET /context` expansion; `path()` bounds a
/// `ThreadPath` job's ancestor-only climb.
pub(crate) struct Walk {
    /// All fetch checks consult this deadline — a shared wall clock,
    /// not per-GET.
    deadline: Instant,
    /// Remote GETs issued so far.
    pub fetches: u32,
    /// `replies` collection pages fetched so far.
    pub pages: u32,
    /// Remote items examined (ancestor steps + collection items).
    pub scanned: u32,
    /// Statuses materialized that weren't local before.
    pub imported: u32,
    /// Set whenever a cap, a dead document or the deadline truncated
    /// the walk — drives "incomplete" reporting.
    pub partial: bool,
    max_fetches: u32,
    max_pages: u32,
    max_chain: u32,
    max_depth: u32,
    /// `replies` items materialized per collection — `u32::MAX` keeps
    /// the full walk unbounded (fetch/page caps still apply).
    max_breadth: u32,
}

impl Walk {
    /// The `fetch_remote_context` budget — the historical caps.
    fn full() -> Self {
        Walk {
            deadline: Instant::now() + DEADLINE,
            fetches: 0,
            pages: 0,
            scanned: 0,
            imported: 0,
            partial: false,
            max_fetches: MAX_FETCHES,
            max_pages: MAX_PAGES,
            max_chain: MAX_CHAIN,
            max_depth: MAX_DEPTH,
            max_breadth: u32::MAX,
        }
    }

    /// The inline `GET /context` budget — small enough to answer
    /// within a request; what it misses continues in a background job.
    pub(crate) fn quick(config: &RemoteContextConfig) -> Self {
        Walk {
            deadline: Instant::now() + Duration::from_millis(config.quick_budget_ms),
            fetches: 0,
            pages: 0,
            scanned: 0,
            imported: 0,
            partial: false,
            max_fetches: config.quick_max_fetches,
            max_pages: 1,
            max_chain: config.path_max,
            max_depth: config.descendant_depth,
            max_breadth: config.descendant_breadth,
        }
    }

    /// The `ThreadExpand` continuation budget — the full walk's wall
    /// clock, GET and page caps, but the configured descendant
    /// depth/breadth (the plan's "background continuation" row:
    /// 60 s, 100 GETs, 20 pages, depth `descendant_depth`).
    pub(crate) fn continuation(config: &RemoteContextConfig) -> Self {
        Walk {
            deadline: Instant::now() + DEADLINE,
            fetches: 0,
            pages: 0,
            scanned: 0,
            imported: 0,
            partial: false,
            max_fetches: MAX_FETCHES,
            max_pages: MAX_PAGES,
            max_chain: MAX_CHAIN,
            max_depth: config.descendant_depth,
            max_breadth: config.descendant_breadth,
        }
    }

    /// The `ThreadPath` job's ancestor-only budget — the replies
    /// subtree of the target stays lazy.
    pub(crate) fn path(config: &RemoteContextConfig) -> Self {
        Walk {
            deadline: Instant::now() + PATH_DEADLINE,
            fetches: 0,
            pages: 0,
            scanned: 0,
            imported: 0,
            partial: false,
            // The ancestor docs plus one slack fetch — the budget
            // maps 1:1 to the chain cap.
            max_fetches: config.path_max + 1,
            max_pages: 0,
            max_chain: config.path_max,
            max_depth: 0,
            max_breadth: 0,
        }
    }

    /// Fetch capacity exhausted (GET cap or wall clock).
    fn exhausted(&self) -> bool {
        self.fetches >= self.max_fetches || Instant::now() >= self.deadline
    }

    /// `replies` pagination capacity exhausted (page cap or wall
    /// clock).
    fn pages_exhausted(&self) -> bool {
        self.pages >= self.max_pages || Instant::now() >= self.deadline
    }
}

/// What the endpoint reports back.
#[derive(Debug, Default)]
pub struct ThreadReport {
    /// Statuses materialized that weren't local before — ancestors
    /// and replies (the target itself doesn't count; re-ingesting it
    /// only repairs its parent link).
    pub imported: u32,
    /// Remote items examined (ancestor steps + collection items).
    pub scanned: u32,
    /// `replies` collection pages fetched.
    pub pages: u32,
    /// `true` when the ancestor chain reached a root (or a locally
    /// continuous parent) and every `replies` collection was walked
    /// to exhaustion inside the budgets — `false` means a cap, a dead
    /// document or the deadline stopped the walk early.
    pub complete: bool,
}

/// `fetch_remote_context` for `status` (a remote row with `uri`).
///
/// Fails fast on a held in-flight lock ([`Error::RateLimited`]) or an
/// unreachable target object; later failures degrade to a partial
/// report instead.
pub async fn fetch_remote_context(ap: &ApContext, status: &StatusRow) -> Result<ThreadReport> {
    let uri = status
        .uri
        .clone()
        .ok_or_else(|| Error::Validation("remote status has no uri".to_string()))?;
    if !claim_lock(ap, status.id).await {
        return Err(Error::RateLimited(
            "a remote context fetch is already running for this status".to_string(),
        ));
    }
    let report = run(ap, &uri, status.id).await;
    release_lock(ap, status.id).await;
    if report.is_ok() {
        // An explicit fetch refreshes the grace markers either way —
        // a partial report still counts as a recent expansion.
        let _ = record_expansion(&ap.core.db, status.id, MAX_DEPTH as i32).await;
    }
    report
}

/// `SET … NX EX` on the per-status lock — one walk expands a thread
/// at a time; a second caller 429s instead of doubling the remote's
/// load.
pub(crate) async fn claim_lock(ap: &ApContext, status_id: i64) -> bool {
    let key = ap
        .core
        .redis
        .key(&format!("remote-thread-fetch:{status_id}"));
    let mut conn = match ap.core.redis.conn().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(status_id, error = %e, "redis unavailable — refusing thread fetch");
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
            tracing::warn!(status_id, error = %e, "thread fetch lock check failed");
            false
        }
    }
}

/// Release the lock — best-effort; the lease lapses on its own.
pub(crate) async fn release_lock(ap: &ApContext, status_id: i64) {
    let key = ap
        .core
        .redis
        .key(&format!("remote-thread-fetch:{status_id}"));
    let Ok(mut conn) = ap.core.redis.conn().await else {
        return;
    };
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&key).query_async(&mut *conn).await;
}

/// Outcome of one remote object GET inside the shared
/// deadline/retry budget — the caller distinguishes a permanent dead
/// end from a retryable failure.
enum Fetch {
    Doc(Value, String),
    /// Permanently gone — a dead end for the walk, not a retry.
    Gone,
    /// 429 or transient after retries — jobs back off through the
    /// queue; the request path degrades to what's stored.
    Transient,
}

async fn fetch(ap: &ApContext, url: &str, walk: &mut Walk) -> Result<Fetch> {
    walk.fetches += 1;
    match outbox::fetch_doc(ap, url, walk.deadline).await? {
        RemoteFetch::Doc(f) => Ok(Fetch::Doc(f.doc, f.url)),
        RemoteFetch::Gone => Ok(Fetch::Gone),
        RemoteFetch::RateLimited | RemoteFetch::Failed => Ok(Fetch::Transient),
    }
}

/// Where a status's `replies` live: an inline page (`orderedItems`/
/// `items` right on the collection object), the collection's `first`
/// page (inline or linked), or a bare collection URL — the fetched
/// document then yields `first`.
fn replies_ref(doc: &Value) -> Option<PageRef> {
    let replies = doc.get("replies")?;
    match replies {
        Value::String(s) if !s.is_empty() => Some(PageRef::Url(s.clone())),
        Value::Object(_) => {
            if replies.get("orderedItems").is_some() || replies.get("items").is_some() {
                return Some(PageRef::Inline(replies.clone()));
            }
            outbox::link_ref(replies, "first")
        }
        _ => None,
    }
}

/// Materialize a remote object document served from `url` as a
/// status. `true` when a new local row landed — a tombstoned row is
/// never resurrected, and [`resolve::materialize_remote`] itself
/// drops untrusted or non-status documents.
async fn ingest(ap: &ApContext, url: &str, doc: &Value) -> Result<bool> {
    let Some(uri) = doc.get("id").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let existing = posts::load_by_uri(&ap.core.db, uri).await?;
    if existing.as_ref().is_some_and(|s| s.tombstone) {
        return Ok(false);
    }
    let hit = crate::search::resolve::materialize_remote(&ap.core.db, ap, url, doc).await?;
    Ok(existing.is_none() && matches!(hit, Some(ResolvedHit::Status(_))))
}

/// Where the ancestor climb starts.
pub(crate) enum AncestorStart<'a> {
    /// Climb from a fetched document's `inReplyTo` — the document
    /// itself is also queued for replies expansion (`fetch_remote_context`,
    /// selection expansion).
    Doc(&'a Value, &'a str),
    /// Climb from a stored row's `in_reply_to_uri` — the `ThreadPath`
    /// job doesn't fetch the target's own doc at all (its replies
    /// subtree stays lazy).
    Uri(&'a str),
}

/// What the climb found: fetched ancestor docs awaiting ingest
/// (child→root — ingest them reversed), docs whose `replies` still
/// need expanding, and whether the chain closed cleanly.
#[derive(Default)]
pub(crate) struct AncestorWalk {
    /// Parent docs fetched during the climb, nearest first — ingest
    /// root-first so every child finds its parent locally.
    pub ancestors: Vec<(Value, String)>,
    /// `(doc, served-from URL, depth)` docs whose `replies` still
    /// need expanding — the target first when `start` was `Doc`.
    pub queue: VecDeque<(Value, String, u32)>,
    /// The chain reached a root, a tombstone, or a locally continuous
    /// parent.
    pub chain_done: bool,
}

/// Climb `inReplyTo` from `start` until a locally continuous parent,
/// a tombstone, a root, `max_chain`, or the walk's deadline. Fetched
/// parent docs land in [`AncestorWalk::ancestors`] and are queued for
/// replies expansion at depth 0.
pub(crate) async fn walk_ancestors(
    ap: &ApContext,
    start: AncestorStart<'_>,
    walk: &mut Walk,
) -> Result<AncestorWalk> {
    let mut out = AncestorWalk::default();
    // The doc the current step reads `inReplyTo` from. A `Doc` start
    // also seeds the expansion queue; a `Uri` start supplies the
    // first parent URI directly (the target row is already local).
    let mut pending_uri: Option<String> = None;
    let mut cur: Option<Value> = None;
    match start {
        AncestorStart::Doc(doc, url) => {
            out.queue.push_back((doc.clone(), url.to_string(), 0));
            cur = Some(doc.clone());
        }
        AncestorStart::Uri(uri) => pending_uri = Some(uri.to_string()),
    }

    for _ in 0..walk.max_chain {
        if walk.exhausted() {
            walk.partial = true;
            break;
        }
        let Some(parent_uri) = pending_uri.take().or_else(|| {
            cur.as_ref()
                .and_then(|d| crate::interactions::object_uri(d.get("inReplyTo")))
        }) else {
            out.chain_done = true;
            break;
        };
        match posts::load_by_uri(&ap.core.db, &parent_uri).await? {
            // The parent's own chain is stored (or it was deleted —
            // a tombstone ends the chain legitimately): the context
            // CTE takes it from here.
            Some(row) if row.in_reply_to_id.is_some() || row.tombstone => {
                out.chain_done = true;
                break;
            }
            // A materialized parent with a NULL in_reply_to_id — the
            // gap continues above it. Fetch its remote doc to keep
            // climbing and to expand its replies; no re-ingest.
            Some(row) => {
                walk.scanned += 1;
                match fetch(ap, row.uri.as_deref().unwrap_or(&parent_uri), walk).await? {
                    Fetch::Doc(doc, url) => {
                        out.queue.push_back((doc.clone(), url.clone(), 0));
                        cur = Some(doc);
                    }
                    // Permanently gone — the chain ends here for good;
                    // retrying can never heal it.
                    Fetch::Gone => {
                        out.chain_done = true;
                        break;
                    }
                    Fetch::Transient => {
                        walk.partial = true;
                        break;
                    }
                }
            }
            // The gap — fetch the parent doc and queue it for ingest.
            None => {
                walk.scanned += 1;
                match fetch(ap, &parent_uri, walk).await? {
                    Fetch::Doc(doc, url) => {
                        out.ancestors.push((doc.clone(), url.clone()));
                        out.queue.push_back((doc.clone(), url, 0));
                        cur = Some(doc);
                    }
                    Fetch::Gone => {
                        out.chain_done = true;
                        break;
                    }
                    Fetch::Transient => {
                        walk.partial = true;
                        break;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Breadth-first `replies` expansion over `queue` — parents before
/// children, so a reply always materializes into an existing local
/// chain. Depth, breadth, page, fetch and deadline caps all live on
/// `walk`; truncation flags `walk.partial` rather than failing.
pub(crate) async fn expand_replies(
    ap: &ApContext,
    queue: &mut VecDeque<(Value, String, u32)>,
    walk: &mut Walk,
) -> Result<()> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut seen_items: HashSet<String> = HashSet::new();
    'expand: while let Some((doc, url, depth)) = queue.pop_front() {
        if walk.exhausted() {
            walk.partial = true;
            break;
        }
        let Some(doc_id) = doc.get("id").and_then(|v| v.as_str()).map(str::to_string) else {
            continue;
        };
        if !visited.insert(doc_id) {
            continue;
        }
        let mut breadth_done = 0u32;
        let mut next = replies_ref(&doc);
        while let Some(page) = next.take() {
            // Only *linked* pages count against the page budget —
            // inline pages cost no GET and must not abort the walk.
            let (pdoc, page_url) = match page {
                PageRef::Inline(d) => (d, url.clone()),
                PageRef::Url(u) => {
                    if walk.pages_exhausted() {
                        walk.partial = true;
                        break;
                    }
                    match outbox::fetch_doc(ap, &u, walk.deadline).await? {
                        RemoteFetch::Doc(f) => {
                            walk.pages += 1;
                            (f.doc, f.url)
                        }
                        RemoteFetch::RateLimited | RemoteFetch::Failed => {
                            walk.partial = true;
                            break;
                        }
                        // A permanently dead page keeps what we have;
                        // a retry can't heal it, so it isn't partial.
                        RemoteFetch::Gone => break,
                    }
                }
            };
            let Some(items) = pdoc
                .get("orderedItems")
                .or_else(|| pdoc.get("items"))
                .and_then(Value::as_array)
            else {
                // The fetched document was the collection root rather
                // than a page — descend into its `first`.
                next = outbox::link_ref(&pdoc, "first");
                if next.is_none() {
                    break;
                }
                continue;
            };
            let mut capped = false;
            for item in items {
                if breadth_done >= walk.max_breadth {
                    walk.partial = true;
                    capped = true;
                    break;
                }
                if walk.exhausted() {
                    walk.partial = true;
                    break 'expand;
                }
                walk.scanned += 1;
                let Some(item_uri) = crate::interactions::object_uri(Some(item)) else {
                    continue;
                };
                if !seen_items.insert(item_uri.clone()) {
                    continue;
                }
                // Already local (or tombstoned — never resurrected);
                // its own subtree stays lazy.
                if posts::load_by_uri(&ap.core.db, &item_uri).await?.is_some() {
                    continue;
                }
                // Inline objects that pass the page's same-origin
                // trust check ingest directly; anything else is
                // fetched from its own origin so `trusted_note`
                // applies. Replies by other authors are wanted —
                // that's what the collection is for.
                let mut new_doc: Option<(Value, String)> = None;
                if item.is_object() {
                    match ingest(ap, &page_url, item).await {
                        Ok(true) => {
                            walk.imported += 1;
                            new_doc = Some((item.clone(), page_url.clone()));
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(error = %e, "reply ingest failed"),
                    }
                }
                if new_doc.is_none() {
                    match fetch(ap, &item_uri, walk).await? {
                        Fetch::Doc(fdoc, furl) => match ingest(ap, &furl, &fdoc).await {
                            Ok(true) => {
                                walk.imported += 1;
                                new_doc = Some((fdoc, furl));
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!(error = %e, "reply ingest failed"),
                        },
                        // A deleted reply is skipped silently; a
                        // transient failure is retryable — flag
                        // partial so a continuation may re-walk.
                        Fetch::Gone => {}
                        Fetch::Transient => walk.partial = true,
                    }
                }
                if new_doc.is_some() {
                    breadth_done += 1;
                }
                if let Some((ndoc, nurl)) = new_doc {
                    if depth < walk.max_depth {
                        queue.push_back((ndoc, nurl, depth + 1));
                    }
                }
            }
            // The breadth cap ends this collection — fetching further
            // pages would discard their items anyway.
            if capped {
                break;
            }
            next = outbox::link_ref(&pdoc, "next");
        }
    }
    Ok(())
}

async fn run(ap: &ApContext, uri: &str, status_id: i64) -> Result<ThreadReport> {
    let mut walk = Walk::full();
    let out = expand(ap, uri, status_id, &mut walk).await?;
    Ok(ThreadReport {
        imported: walk.imported,
        scanned: walk.scanned,
        pages: walk.pages,
        complete: out.chain_done && !walk.partial,
    })
}

// ---------------------------------------------------------------------------
// Shared expansion core — selection expansion, `fetch_remote_context`
// and the `ThreadExpand` continuation all run this pipeline under
// different budgets.
// ---------------------------------------------------------------------------

/// What one expansion pass produced — the caller decides whether a
/// background continuation is needed.
#[derive(Debug, Default)]
pub(crate) struct SelectionOutcome {
    /// New statuses materialized this pass.
    pub imported: u32,
    /// The ancestor climb reached a root / continuous local parent /
    /// tombstone.
    pub chain_done: bool,
    /// The walk was truncated (a cap, a dead document or the deadline).
    pub partial: bool,
}

/// One expansion pass over `uri`: fetch the target doc, climb
/// `inReplyTo`, ingest ancestors root-first (then re-ingest the
/// target so its parent link repairs), refresh the remote engagement
/// counters, then breadth-first `replies` expansion — all inside
/// `walk`'s budget. `status_id` keys the counters row (pass `0` for
/// walks that shouldn't touch counters).
async fn expand(
    ap: &ApContext,
    uri: &str,
    status_id: i64,
    walk: &mut Walk,
) -> Result<SelectionOutcome> {
    // The target's own object — `inReplyTo`/`replies` aren't stored
    // locally, so this fetch is always required. A permanently gone
    // target is `NotFound` (the continuation job acks it); a
    // transient failure stays retryable.
    let (target, target_url) = match fetch(ap, uri, walk).await? {
        Fetch::Doc(d, u) => (d, u),
        Fetch::Gone => return Err(Error::NotFound("remote status".to_string())),
        Fetch::Transient => {
            return Err(Error::RateLimited(
                "remote status fetch deferred — origin is rate-limiting or failing".to_string(),
            ))
        }
    };

    let climbed = walk_ancestors(ap, AncestorStart::Doc(&target, &target_url), walk).await?;
    let mut queue = climbed.queue;

    // Ingest ancestors root-first — every child materializes with its
    // parent already local, so `in_reply_to_id` links land.
    for (doc, url) in climbed.ancestors.iter().rev() {
        match ingest(ap, url, doc).await {
            Ok(true) => walk.imported += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "thread ancestor ingest failed"),
        }
    }
    // Re-ingest the target — repairs the NULL in_reply_to_id it was
    // stored with now that the parent row exists.
    if let Err(e) = ingest(ap, &target_url, &target).await {
        tracing::warn!(error = %e, "thread target re-ingest failed");
    }
    // Engagement counters ride the expansion — the target doc carries
    // the `likes`/`shares` collection refs.
    if let Err(e) = maybe_refresh_counts(ap, status_id, &target, walk).await {
        tracing::warn!(status_id, error = %e, "remote counts refresh failed");
    }

    expand_replies(ap, &mut queue, walk).await?;

    Ok(SelectionOutcome {
        imported: walk.imported,
        chain_done: climbed.chain_done,
        partial: walk.partial,
    })
}

// ---------------------------------------------------------------------------
// Selection-time expansion (`GET /context` + `ThreadExpand`)
// ---------------------------------------------------------------------------

/// Whether `status_id`'s expansion is missing or stale — the
/// `/context` trigger. `subtree_done_at` is the freshness signal: a
/// path-only run (`ThreadPath`) leaves it NULL so the first read
/// still expands descendants.
pub(crate) async fn expansion_stale(
    db: &sqlx::PgPool,
    status_id: i64,
    grace_seconds: u64,
) -> Result<bool> {
    let done_at: Option<Option<DateTime<Utc>>> =
        sqlx::query_scalar("SELECT subtree_done_at FROM status_expansions WHERE status_id = $1")
            .bind(status_id)
            .fetch_optional(db)
            .await?;
    Ok(match done_at.flatten() {
        None => true,
        Some(ts) => (Utc::now() - ts) > chrono::Duration::seconds(grace_seconds as i64),
    })
}

/// Mark the ancestor path done for `status_id` — written by the
/// `ThreadPath` job after relinking. `subtree_done_at` stays NULL so
/// a `/context` read still expands the replies subtree.
pub(crate) async fn record_path_done(db: &sqlx::PgPool, status_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO status_expansions (status_id, path_done_at) VALUES ($1, now()) \
         ON CONFLICT (status_id) DO UPDATE SET path_done_at = EXCLUDED.path_done_at",
    )
    .bind(status_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark a full expansion done for `status_id` — both the ancestor
/// path and the replies subtree are fresh until the grace window
/// closes.
async fn record_expansion(db: &sqlx::PgPool, status_id: i64, depth_reached: i32) -> Result<()> {
    sqlx::query(
        "INSERT INTO status_expansions \
             (status_id, path_done_at, subtree_done_at, depth_reached) \
         VALUES ($1, now(), now(), $2) \
         ON CONFLICT (status_id) DO UPDATE SET \
             path_done_at = EXCLUDED.path_done_at, \
             subtree_done_at = EXCLUDED.subtree_done_at, \
             depth_reached = EXCLUDED.depth_reached",
    )
    .bind(status_id)
    .bind(depth_reached)
    .execute(db)
    .await?;
    Ok(())
}

/// `remote-context-cooloff:{status_id}` lease — after a failed
/// selection expansion, a dead origin isn't retried on every read.
const COOLOFF_SECONDS: usize = 60;

/// `true` while a failed expansion's cool-off holds — the `/context`
/// handler skips the walk and serves stored rows.
pub(crate) async fn expansion_cooling(ap: &ApContext, status_id: i64) -> bool {
    let Ok(mut conn) = ap.core.redis.conn().await else {
        return false;
    };
    redis::cmd("EXISTS")
        .arg(
            ap.core
                .redis
                .key(&format!("remote-context-cooloff:{status_id}")),
        )
        .query_async::<i64>(&mut *conn)
        .await
        .unwrap_or(0)
        > 0
}

/// Mark a failed expansion — reads skip the walk for
/// [`COOLOFF_SECONDS`]. Lock contention also marks it: the in-flight
/// walk already covers the pending refresh.
pub(crate) async fn mark_expansion_cooloff(ap: &ApContext, status_id: i64) {
    let Ok(mut conn) = ap.core.redis.conn().await else {
        return;
    };
    let _: redis::RedisResult<()> = redis::cmd("SET")
        .arg(
            ap.core
                .redis
                .key(&format!("remote-context-cooloff:{status_id}")),
        )
        .arg("1")
        .arg("EX")
        .arg(COOLOFF_SECONDS)
        .query_async(&mut *conn)
        .await;
}

/// `GET /context` trigger — a bounded inline expansion under the
/// quick budget; whatever it doesn't reach continues in a
/// `ThreadExpand` job. Every failure mode (including a held lock)
/// degrades to serving what's stored — a read endpoint never 429s.
pub(crate) async fn expand_for_selection(
    ap: &ApContext,
    status: &StatusRow,
) -> Result<SelectionOutcome> {
    let uri = status
        .uri
        .clone()
        .ok_or_else(|| Error::Validation("remote status has no uri".to_string()))?;
    if !claim_lock(ap, status.id).await {
        return Err(Error::RateLimited(
            "a remote context fetch is already running for this status".to_string(),
        ));
    }
    let mut walk = Walk::quick(&ap.core.config.remote_context);
    let out = expand(ap, &uri, status.id, &mut walk).await;
    release_lock(ap, status.id).await;
    let out = out?;
    tracing::debug!(
        status_id = status.id,
        imported = out.imported,
        partial = out.partial,
        "selection expansion pass done"
    );
    // Record even a partial quick pass — the continuation job finishes
    // the work, and repeated /context reads shouldn't re-run the walk.
    let _ = record_expansion(
        &ap.core.db,
        status.id,
        ap.core.config.remote_context.descendant_depth as i32,
    )
    .await;
    if out.partial || !out.chain_done {
        if let Err(e) = ap
            .core
            .dispatcher
            .enqueue(
                JobKind::ThreadExpand,
                json!({"status_id": status.id, "round": 0}),
            )
            .await
        {
            tracing::warn!(
                status_id = status.id,
                error = %e,
                "thread-expand continuation enqueue failed"
            );
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Remote engagement counters — `likes`/`shares`/`quotes` totals.
// ---------------------------------------------------------------------------

/// Refresh the remote like/boost/quote totals on `status_id` from its
/// freshly fetched document — at most once per `counts_grace_seconds`.
/// `likes`/`shares`/`quotes` may carry `totalItems` inline (no GET
/// needed); a bare collection link costs one GET each, charged to
/// `walk`'s fetch budget. Unreached collections stay NULL — never 0
/// — and a refresh that fetched nothing leaves the row alone so the
/// next expansion retries.
async fn maybe_refresh_counts(
    ap: &ApContext,
    status_id: i64,
    target_doc: &Value,
    walk: &mut Walk,
) -> Result<()> {
    if status_id == 0 {
        return Ok(());
    }
    let fetched_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT remote_counts_fetched_at FROM statuses WHERE id = $1")
            .bind(status_id)
            .fetch_optional(&ap.core.db)
            .await?
            .flatten();
    let grace = ap.core.config.remote_context.counts_grace_seconds as i64;
    if let Some(ts) = fetched_at {
        if (Utc::now() - ts).num_seconds() < grace {
            return Ok(());
        }
    }
    let mut likes = None;
    let mut shares = None;
    let mut quotes = None;
    for (key, slot) in [
        ("likes", &mut likes),
        ("shares", &mut shares),
        ("quotes", &mut quotes),
    ] {
        let Some(coll) = target_doc.get(key) else {
            continue;
        };
        // An inline `totalItems` needs no GET.
        if let Some(t) = lookup::total_items(coll) {
            *slot = Some(t as i32);
            continue;
        }
        let url = match coll {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(o) => o
                .get("id")
                .or_else(|| o.get("href"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            _ => None,
        };
        let Some(url) = url else {
            continue;
        };
        if walk.exhausted() {
            walk.partial = true;
            break;
        }
        if let Fetch::Doc(cdoc, _) = fetch(ap, &url, walk).await? {
            *slot = lookup::total_items(&cdoc).map(|t| t as i32);
        }
    }
    if likes.is_none() && shares.is_none() && quotes.is_none() {
        return Ok(());
    }
    // COALESCE keeps the last fetched total when a refresh misses one
    // collection — a dead `shares` link must not erase a prior count.
    sqlx::query(
        "UPDATE statuses SET \
             remote_likes_count = COALESCE($2, remote_likes_count), \
             remote_shares_count = COALESCE($3, remote_shares_count), \
             remote_quotes_count = COALESCE($4, remote_quotes_count), \
             remote_counts_fetched_at = now() \
         WHERE id = $1",
    )
    .bind(status_id)
    .bind(likes)
    .bind(shares)
    .bind(quotes)
    .execute(&ap.core.db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Expansion jobs — `ThreadExpand` (selection continuation) and
// `ThreadPath` (followed-author reply paths).
// ---------------------------------------------------------------------------

/// Rounds a `ThreadExpand` continuation may chain — a thread that
/// still overflows the full budget after this many rounds stops
/// retrying (the rest stays lazy; selecting deeper expands it).
const MAX_CONTINUATION_ROUNDS: u32 = 3;

/// `JobKind::ThreadExpand` — the background continuation of a
/// selection expansion that didn't finish inside the quick budget.
/// Runs the shared `expand` under the full walk budget; permanently
/// gone targets ack (a retry can't help), transient failures retry
/// through the queue's backoff.
pub struct ThreadExpandJob {
    ap: ApContext,
}

impl ThreadExpandJob {
    /// Create the handler.
    pub fn new(ap: ApContext) -> Self {
        ThreadExpandJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ThreadExpandJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(status_id) = payload.get("status_id").and_then(Value::as_i64) else {
            return Ok(());
        };
        let ap = &self.ap;
        let Some(status) = posts::load(&ctx.db, status_id).await? else {
            return Ok(());
        };
        let Some(uri) = status.uri.clone() else {
            return Ok(());
        };
        if !claim_lock(ap, status.id).await {
            // Another expansion is in flight — retry after the queue's
            // backoff; the lock lease lapses on its own.
            return Err(Error::RateLimited(
                "a remote context fetch is already running for this status".to_string(),
            ));
        }
        let cfg = &ctx.config.remote_context;
        let mut walk = Walk::continuation(cfg);
        let out = expand(ap, &uri, status.id, &mut walk).await;
        release_lock(ap, status.id).await;
        let out = match out {
            // Permanently gone — settled, not a retry.
            Err(Error::NotFound(_)) => return Ok(()),
            other => other?,
        };
        record_expansion(&ctx.db, status.id, cfg.descendant_depth as i32).await?;
        if out.partial || !out.chain_done {
            let round = payload.get("round").and_then(Value::as_u64).unwrap_or(0) as u32;
            if round < MAX_CONTINUATION_ROUNDS {
                ap.core
                    .dispatcher
                    .enqueue(
                        JobKind::ThreadExpand,
                        json!({"status_id": status_id, "round": round + 1}),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

/// `JobKind::ThreadPath` — ancestor-path materialization for a
/// followed author's reply whose parent wasn't local at ingest.
/// Ancestors only (the target's replies subtree stays lazy); on
/// success the reply is relinked and fanned out — the delayed
/// `TimelineFanOut` enqueued at ingest is the fallback for a slow or
/// failed walk (fan-out is an idempotent `ZADD`).
pub struct ThreadPathJob {
    ap: ApContext,
}

impl ThreadPathJob {
    /// Create the handler.
    pub fn new(ap: ApContext) -> Self {
        ThreadPathJob { ap }
    }
}

#[async_trait::async_trait]
impl JobHandler for ThreadPathJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let Some(status_id) = payload.get("status_id").and_then(Value::as_i64) else {
            return Ok(());
        };
        let ap = &self.ap;
        let Some(status) = posts::load(&ctx.db, status_id).await? else {
            return Ok(());
        };
        // Nothing to do: already linked, tombstoned, or not a reply.
        if status.in_reply_to_id.is_some() || status.tombstone {
            return Ok(());
        }
        let Some(parent_uri) = status.in_reply_to_uri.clone() else {
            return Ok(());
        };
        if !claim_lock(ap, status.id).await {
            return Err(Error::RateLimited(
                "a remote context fetch is already running for this status".to_string(),
            ));
        }
        let mut walk = Walk::path(&ctx.config.remote_context);
        let res: Result<()> = async {
            let climbed = walk_ancestors(ap, AncestorStart::Uri(&parent_uri), &mut walk).await?;
            for (doc, url) in climbed.ancestors.iter().rev() {
                if let Err(e) = ingest(ap, url, doc).await {
                    tracing::warn!(error = %e, "thread-path ancestor ingest failed");
                }
            }
            // Relink the target now the parent row exists — the
            // `replies_count` move mirrors `upsert`'s counter logic
            // (NULL → linked bumps the parent). Both updates share one
            // transaction: a crash between them would link without
            // counting, and a retry (which sees `in_reply_to_id` set)
            // would never re-bump.
            let mut tx = ctx.db.begin().await?;
            let relinked: Option<i64> = sqlx::query_scalar(
                "UPDATE statuses s SET in_reply_to_id = p.id, \
                     in_reply_to_account_id = p.account_id, updated_at = now() \
                 FROM statuses p \
                 WHERE s.id = $1 AND s.in_reply_to_id IS NULL \
                   AND p.uri = s.in_reply_to_uri AND NOT p.tombstone \
                 RETURNING p.id",
            )
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(parent_id) = relinked {
                sqlx::query("UPDATE statuses SET replies_count = replies_count + 1 WHERE id = $1")
                    .bind(parent_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            record_path_done(&ctx.db, status_id).await?;
            Ok(())
        }
        .await;
        release_lock(ap, status.id).await;
        res?;
        // Gated reveal — ingest withheld both the fan-out and the
        // stream `update` for this reply; it appears now, with its
        // parent link materialized. The ingest-time delayed fallback
        // covers walks that ran long.
        crate::timelines::fanout::enqueue(&ap.core.dispatcher, status_id).await;
        if let Some(row) = posts::load(&ctx.db, status_id).await? {
            let pctx = crate::posts::PostContext::from_job(ctx, Some(ap.clone()));
            crate::timelines::stream::publish_update(&pctx, &row).await;
        }
        Ok(())
    }
}

/// Register this module's job handlers.
pub(crate) fn register(registry: &mut crate::jobs::Registry, ap: &ApContext) {
    registry.register(JobKind::ThreadExpand, ThreadExpandJob::new(ap.clone()));
    registry.register(JobKind::ThreadPath, ThreadPathJob::new(ap.clone()));
}
