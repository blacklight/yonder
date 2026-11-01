//! Remote thread-context fetch — the lazy "expand thread" behind
//! `POST /api/v1/statuses/{id}/fetch_remote_context`.
//!
//! A remote reply often arrives with a broken local chain: its
//! `inReplyTo` was never materialized (the URI isn't stored locally),
//! so `posts::context` sees no ancestors and the user only gets the
//! one post. When a user opens such a status the frontend calls this
//! endpoint, which walks the *remote* thread and materializes it
//! through the same ingest path an inbox delivery would take
//! (`search::resolve::materialize_remote` — per-origin trust checks
//! included):
//!
//! * **Ancestors** — re-fetch the status's own object, then climb
//!   `inReplyTo`. A locally-known parent whose `in_reply_to_id` is
//!   still `NULL` means the gap continues above it — its remote doc
//!   is fetched to keep climbing (no re-ingest needed). Ancestor docs
//!   are ingested root-first so every child finds its parent locally;
//!   the target is re-ingested last, which repairs the `NULL`
//!   `in_reply_to_id` it was stored with (the `ON CONFLICT (uri)`
//!   upsert rewrites the link).
//! * **Replies** — breadth-first expansion of the target's and each
//!   fetched ancestor's `replies` collection (inline items or linked
//!   pages). Collection items are materialized by URI: an inline
//!   object that passes the same-origin trust check for the page it
//!   arrived in ingests directly; anything else is fetched from its
//!   own origin first — replies by other authors are the point of the
//!   collection, so no ownership check applies (unlike outbox sync).
//!
//! Bounds: every remote GET goes through [`outbox::fetch_doc`]
//! (signed, 429/transient → bounded backoff); the whole walk shares a
//! wall-clock deadline, a per-status Redis lock and caps on chain
//! length, collection pages, remote GETs and expansion depth — a
//! hostile or huge thread degrades to a partial report, never a hung
//! request. Anything not reached stays lazy: opening that status
//! expands its own context.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::posts::{self, StatusRow};
use crate::search::resolve::ResolvedHit;
use crate::{Error, Result};

use super::lookup::RemoteFetch;
use super::outbox::{self, PageRef};
use super::ApContext;

/// Whole-walk wall clock — the endpoint answers within it; what was
/// materialized stays materialized (partial report).
const DEADLINE: Duration = Duration::from_secs(60);
/// Ancestor-climb cap — matches `posts::context`'s walk depth, so a
/// corrupted `inReplyTo` cycle can't spin the fetch.
const MAX_CHAIN: u32 = 64;
/// Replies-of-replies expansion depth — deeper nesting stays lazy
/// (opening that status expands its own context).
const MAX_DEPTH: u32 = 10;
/// Remote object GETs per fetch — the target doc, ancestor docs and
/// replies materialized from their own origin.
const MAX_FETCHES: u32 = 100;
/// `replies` collection pages per fetch.
const MAX_PAGES: u32 = 20;
/// In-flight lock lease — the deadline plus margin; released on
/// completion so a successful fetch doesn't throttle the next.
const LOCK_TTL_SECONDS: usize = (DEADLINE.as_secs() + 60) as usize;

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
    let report = run(ap, &uri).await;
    release_lock(ap, status.id).await;
    report
}

/// `SET … NX EX` on the per-status lock — one walk expands a thread
/// at a time; a second caller 429s instead of doubling the remote's
/// load.
async fn claim_lock(ap: &ApContext, status_id: i64) -> bool {
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
async fn release_lock(ap: &ApContext, status_id: i64) {
    let key = ap
        .core
        .redis
        .key(&format!("remote-thread-fetch:{status_id}"));
    let Ok(mut conn) = ap.core.redis.conn().await else {
        return;
    };
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&key).query_async(&mut *conn).await;
}

/// One remote object GET inside the shared deadline/retry budget.
/// `None` = permanently gone or still failing after retries — a dead
/// end for the walk, not a hard error.
async fn fetch(
    ap: &ApContext,
    url: &str,
    deadline: Instant,
    fetches: &mut u32,
) -> Result<Option<(Value, String)>> {
    *fetches += 1;
    match outbox::fetch_doc(ap, url, deadline).await? {
        RemoteFetch::Doc(f) => Ok(Some((f.doc, f.url))),
        RemoteFetch::Gone | RemoteFetch::RateLimited | RemoteFetch::Failed => Ok(None),
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

async fn run(ap: &ApContext, uri: &str) -> Result<ThreadReport> {
    let deadline = Instant::now() + DEADLINE;
    let mut report = ThreadReport::default();
    let mut fetches = 0u32;
    // Set whenever a cap, a dead document or the deadline truncated
    // the walk — drives `complete`.
    let mut partial = false;

    // The target's own object — `inReplyTo`/`replies` aren't stored
    // locally, so this fetch is always required.
    let Some((target, target_url)) = fetch(ap, uri, deadline, &mut fetches).await? else {
        return Err(Error::NotFound("remote status".to_string()));
    };

    // (doc, served-from URL, depth) — docs whose `replies` still need
    // expanding. The target goes first so its direct replies land
    // before ancestor-side threads.
    let mut queue: VecDeque<(Value, String, u32)> = VecDeque::new();
    queue.push_back((target.clone(), target_url.clone(), 0));
    // Ancestor docs awaiting ingest, child→root (ingested reversed).
    let mut ancestors: Vec<(Value, String)> = Vec::new();
    let mut chain_done = false;

    let mut cur = target.clone();
    for _ in 0..MAX_CHAIN {
        if fetches >= MAX_FETCHES || Instant::now() >= deadline {
            partial = true;
            break;
        }
        let Some(parent_uri) = crate::interactions::object_uri(cur.get("inReplyTo")) else {
            chain_done = true;
            break;
        };
        match posts::load_by_uri(&ap.core.db, &parent_uri).await? {
            // The parent's own chain is stored (or it was deleted —
            // a tombstone ends the chain legitimately): the context
            // CTE takes it from here.
            Some(row) if row.in_reply_to_id.is_some() || row.tombstone => {
                chain_done = true;
                break;
            }
            // A materialized parent with a NULL in_reply_to_id — the
            // gap continues above it. Fetch its remote doc to keep
            // climbing and to expand its replies; no re-ingest.
            Some(row) => {
                report.scanned += 1;
                let Some((doc, url)) = fetch(
                    ap,
                    row.uri.as_deref().unwrap_or(&parent_uri),
                    deadline,
                    &mut fetches,
                )
                .await?
                else {
                    partial = true;
                    break;
                };
                queue.push_back((doc.clone(), url, 0));
                cur = doc;
            }
            // The gap — fetch the parent doc and queue it for ingest.
            None => {
                report.scanned += 1;
                let Some((doc, url)) = fetch(ap, &parent_uri, deadline, &mut fetches).await? else {
                    partial = true;
                    break;
                };
                ancestors.push((doc.clone(), url.clone()));
                queue.push_back((doc.clone(), url, 0));
                cur = doc;
            }
        }
    }

    // Ingest ancestors root-first — every child materializes with its
    // parent already local, so `in_reply_to_id` links land.
    for (doc, url) in ancestors.iter().rev() {
        match ingest(ap, url, doc).await {
            Ok(true) => report.imported += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "thread ancestor ingest failed"),
        }
    }
    // Re-ingest the target — repairs the NULL in_reply_to_id it was
    // stored with now that the parent row exists.
    if let Err(e) = ingest(ap, &target_url, &target).await {
        tracing::warn!(error = %e, "thread target re-ingest failed");
    }

    // Breadth-first `replies` expansion — parents before children, so
    // a reply always materializes into an existing local chain.
    let mut visited: HashSet<String> = HashSet::new();
    let mut seen_items: HashSet<String> = HashSet::new();
    'expand: while let Some((doc, url, depth)) = queue.pop_front() {
        if fetches >= MAX_FETCHES || Instant::now() >= deadline {
            partial = true;
            break;
        }
        let Some(doc_id) = doc.get("id").and_then(|v| v.as_str()).map(str::to_string) else {
            continue;
        };
        if !visited.insert(doc_id) {
            continue;
        }
        let mut next = replies_ref(&doc);
        while let Some(page) = next.take() {
            if report.pages >= MAX_PAGES || Instant::now() >= deadline {
                partial = true;
                break 'expand;
            }
            let (pdoc, page_url) = match page {
                PageRef::Inline(d) => (d, url.clone()),
                PageRef::Url(u) => match outbox::fetch_doc(ap, &u, deadline).await? {
                    RemoteFetch::Doc(f) => {
                        report.pages += 1;
                        (f.doc, f.url)
                    }
                    // A dead page keeps what we have — partial report.
                    RemoteFetch::Gone | RemoteFetch::RateLimited | RemoteFetch::Failed => {
                        partial = true;
                        break;
                    }
                },
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
            for item in items {
                if fetches >= MAX_FETCHES || Instant::now() >= deadline {
                    partial = true;
                    break 'expand;
                }
                report.scanned += 1;
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
                            report.imported += 1;
                            new_doc = Some((item.clone(), page_url.clone()));
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(error = %e, "reply ingest failed"),
                    }
                }
                if new_doc.is_none() {
                    if let Some((fdoc, furl)) = fetch(ap, &item_uri, deadline, &mut fetches).await?
                    {
                        match ingest(ap, &furl, &fdoc).await {
                            Ok(true) => {
                                report.imported += 1;
                                new_doc = Some((fdoc, furl));
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!(error = %e, "reply ingest failed"),
                        }
                    }
                }
                if let Some((ndoc, nurl)) = new_doc {
                    if depth < MAX_DEPTH {
                        queue.push_back((ndoc, nurl, depth + 1));
                    }
                }
            }
            next = outbox::link_ref(&pdoc, "next");
        }
    }

    report.complete = chain_done && !partial && queue.is_empty();
    Ok(report)
}
