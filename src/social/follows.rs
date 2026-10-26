//! Follow/unfollow REST-facing services — thin wrappers over
//! [`crate::users::relationships`] that add the notification events
//! and the paginated follower/following listings.

use std::collections::{HashMap, HashSet};

use futures_util::StreamExt;
use serde_json::Value;
use sqlx::{PgPool, Row};

use crate::{
    activitypub::{lookup, moderation, ApContext},
    http::{paginate::Page, AppState},
    interactions::emit_notification_event,
    users::relationships::{self, Ctx},
    Error, Result,
};

use super::{FollowOpts, FollowOutcome};

/// Follow `target_id` on behalf of `follower_id`, emitting the
/// `follow`/`follow_request` notification event for local targets
/// ([`emit_notification_event`] drops remote recipients itself —
/// a remote follow is announced to the remote by the AP activity,
/// not by our notification pipeline).
pub async fn follow(
    state: &AppState,
    follower_id: i64,
    target_id: i64,
    opts: FollowOpts,
) -> Result<FollowOutcome> {
    let cfg = state.config();
    let ctx = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
    let outcome = relationships::follow(ctx, follower_id, target_id, opts).await?;
    let kind = match outcome {
        FollowOutcome::Following => Some("follow"),
        FollowOutcome::Requested => Some("follow_request"),
    };
    if let Some(kind) = kind {
        emit_notification_event(
            &state.db,
            &state.dispatcher,
            kind,
            follower_id,
            None,
            target_id,
        )
        .await;
    }
    if matches!(outcome, FollowOutcome::Following) {
        // A landed follow seeds the follower's home index with the
        // followee's recent statuses. A `Requested` remote follow
        // backfills later, off the inbound `Accept`.
        crate::timelines::fanout::enqueue_backfill(&state.dispatcher, follower_id, target_id).await;
    }
    Ok(outcome)
}

/// Unfollow `target_id`; returns whether a row was removed.
pub async fn unfollow(state: &AppState, follower_id: i64, target_id: i64) -> Result<bool> {
    let cfg = state.config();
    let ctx = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
    relationships::unfollow(ctx, follower_id, target_id).await
}

/// `POST /api/v1/follows` (deprecated): resolve `uri` — a
/// `user@domain` handle, bare local username or actor URL — then
/// follow. Returns the target's `accounts.id`.
pub async fn follow_by_uri(state: &AppState, follower_id: i64, uri: &str) -> Result<i64> {
    let cfg = state.config();
    let ctx = Ctx::for_parts(&state.db, &cfg, state.ap.as_ref());
    let target_id = relationships::resolve_account(ctx, uri).await?;
    follow(state, follower_id, target_id, FollowOpts::default()).await?;
    Ok(target_id)
}

/// `(account_id, cursor)` pages for followers/following lists —
/// newest `follows` rows first, cursor is `follows.id`.
async fn edge_page(
    db: &PgPool,
    direction: &str,
    account_id: i64,
    page: &Page,
) -> Result<Vec<(i64, i64)>> {
    // $1 = the listed account; cursor/filter params follow.
    let query = format!(
        "SELECT a.id AS account_id, f.id AS cursor FROM follows f \
         JOIN accounts a ON a.id = f.{direction} \
         WHERE f.{other} = $1 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
           AND ($2::bigint IS NULL OR f.id < $2) \
           AND ($3::bigint IS NULL OR f.id > $3) \
         ORDER BY f.id DESC LIMIT $4",
        direction = direction,
        other = other_column(direction),
    );
    let rows = sqlx::query(&query)
        .bind(account_id)
        .bind(page.max_id)
        .bind(page.since_id)
        .bind(page.limit)
        .fetch_all(db)
        .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("account_id"), r.get("cursor")))
        .collect())
}

fn other_column(direction: &str) -> &'static str {
    match direction {
        "account_id" => "target_account_id",
        "target_account_id" => "account_id",
        _ => unreachable!("direction is a fixed literal"),
    }
}

/// Accounts following `target_id` (its followers).
pub async fn followers(db: &PgPool, target_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_page(db, "account_id", target_id, page).await
}

/// Accounts `account_id` follows.
pub async fn following(db: &PgPool, account_id: i64, page: &Page) -> Result<Vec<(i64, i64)>> {
    edge_page(db, "target_account_id", account_id, page).await
}

/// Whether `hide_network` keeps the caller out of `account`'s
/// follower/following listings — the owner and admins see through it.
pub async fn network_hidden(db: &PgPool, account_id: i64, viewer: Option<i64>) -> Result<bool> {
    if viewer == Some(account_id) {
        return Ok(false);
    }
    let hidden: bool =
        sqlx::query_scalar("SELECT COALESCE(hide_network, false) FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    Ok(hidden)
}

/// Load a visible target account for the `accounts/{id}` actions, or
/// 404 — nonexistent, suspended and deleted accounts are all absent
/// from relationship writes.
pub async fn visible_target(db: &PgPool, account_id: i64) -> Result<()> {
    if !super::visibility::visible(db, account_id).await? {
        return Err(Error::NotFound("account".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Remote network listings (best-effort)
// ---------------------------------------------------------------------------

/// How long a synced remote followers/following list is served before
/// the next read re-fetches the remote collection.
const REMOTE_NETWORK_TTL_SECONDS: i64 = 60 * 60;
/// Members persisted per (account, direction) — the remote first page,
/// capped. Deeper remote pages are not followed; the listing stays
/// best-effort.
const REMOTE_NETWORK_CAP: usize = 80;
/// Unknown member actors fetched synchronously per sync — the rest get
/// a stub `accounts` row plus a `RemoteActorFetch` job so the request
/// stays bounded.
const REMOTE_NETWORK_SYNC_FETCH: usize = 24;
/// Parallel remote actor fetches — pubby's client is synchronous.
const REMOTE_NETWORK_FETCH_CONCURRENCY: usize = 8;

/// Whether `account_id` is a remote account — and, when its cached
/// `direction` (`"followers"`/`"following"`) listing is stale or
/// missing, refresh it inline from the remote collection. Never fails
/// the caller: every error degrades to "serve what's stored".
pub async fn ensure_remote_network(
    state: &AppState,
    account_id: i64,
    direction: &'static str,
) -> bool {
    let row = sqlx::query(
        "SELECT actor_id, actor_data FROM accounts \
         WHERE id = $1 AND domain IS NOT NULL AND deleted_at IS NULL",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;
    let (actor_id, actor_data) = match row {
        Ok(Some(r)) => (
            r.get::<String, _>("actor_id"),
            r.get::<Value, _>("actor_data"),
        ),
        // Local/missing account (caller falls back to `follows`) or a
        // DB error — report "not remote" either way; the real error
        // surfaces when the caller runs its own query.
        _ => return false,
    };
    let Some(ap) = state.ap.as_ref() else {
        return true;
    };
    match claim_network_sync(&state.db, account_id, direction).await {
        Ok(true) => {
            if let Err(e) =
                sync_remote_network(state, ap, account_id, direction, &actor_id, actor_data).await
            {
                tracing::warn!(
                    account_id,
                    direction,
                    error = %e,
                    "remote network sync failed"
                );
            }
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(account_id, direction, error = %e, "remote network claim failed");
        }
    }
    true
}

/// Claim the (account, direction) sync slot: `true` when the listing
/// was stale (or never synced) and this caller now owns the refresh.
/// The stamp lands up front, so a failed fetch is throttled too —
/// a dead remote must not be re-requested on every page view.
async fn claim_network_sync(db: &PgPool, account_id: i64, direction: &str) -> Result<bool> {
    let claimed = sqlx::query_scalar::<_, i64>(
        "INSERT INTO remote_network_syncs (account_id, direction, synced_at) \
         VALUES ($1, $2, now()) \
         ON CONFLICT (account_id, direction) DO UPDATE SET synced_at = now() \
         WHERE remote_network_syncs.synced_at < now() - make_interval(secs => $3) \
         RETURNING account_id",
    )
    .bind(account_id)
    .bind(direction)
    .bind(REMOTE_NETWORK_TTL_SECONDS)
    .fetch_optional(db)
    .await?;
    Ok(claimed.is_some())
}

/// The actor document's `followers`/`following` collection URL —
/// plain string or `{id: …}` reference.
fn collection_uri(actor_data: &Value, direction: &str) -> Option<String> {
    match actor_data.get(direction) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(m)) => m
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    }
}

/// Actor URIs out of a collection/page document: `orderedItems`/`items`
/// string-or-`{id}` entries, descending into an embedded `first` page.
/// `None` when the document carries no items at all (e.g. a bare
/// `totalItems` count or a `first` page URL, which the caller fetches).
fn collection_member_uris(doc: &Value) -> Option<Vec<String>> {
    for key in ["orderedItems", "items"] {
        if let Some(arr) = doc.get(key).and_then(|v| v.as_array()) {
            return Some(
                arr.iter()
                    .filter_map(|i| match i {
                        Value::String(s) if !s.is_empty() => Some(s.clone()),
                        Value::Object(m) => m
                            .get("id")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                        _ => None,
                    })
                    .collect(),
            );
        }
    }
    doc.get("first")
        .filter(|f| f.is_object())
        .and_then(collection_member_uris)
}

/// Refresh one remote account's `direction` edge list: fetch the
/// collection document, materialize the member actors it names, then
/// atomically replace the stored edges — remote members in remote
/// order, then local `follows` edges the remote list omitted.
async fn sync_remote_network(
    state: &AppState,
    ap: &ApContext,
    account_id: i64,
    direction: &'static str,
    actor_id: &str,
    actor_data: Value,
) -> Result<()> {
    // 1. Collection URL — cached actor docs and fresh stub rows may
    // lack it; refetch the actor once before giving up.
    let mut collection = collection_uri(&actor_data, direction);
    if collection.is_none() {
        let handler = ap.multi.instance.clone();
        let url = actor_id.to_string();
        if let Ok(Some(doc)) =
            tokio::task::spawn_blocking(move || handler.outbox.fetch_actor(&url)).await
        {
            crate::activitypub::storage::upsert_remote_actor_row(&state.db, actor_id, &doc).await?;
            collection = collection_uri(&doc, direction);
        }
    }
    let Some(collection) = collection else {
        return Ok(());
    };

    // 2. The collection's first page of member URIs. `first` may be a
    // URL to a page document instead of an embedded page — one extra
    // fetch, no deeper traversal. A failed fetch (or a count-only
    // document) leaves the previous edges alone.
    let Some(doc) = lookup::fetch_remote_object(ap, &collection).await? else {
        return Ok(());
    };
    let mut uris = collection_member_uris(&doc);
    if uris.is_none() {
        if let Some(first) = doc.get("first").and_then(|f| f.as_str()) {
            if let Some(page) = lookup::fetch_remote_object(ap, first).await? {
                uris = collection_member_uris(&page);
            }
        }
    }
    let Some(uris) = uris else {
        return Ok(());
    };

    // 3. Drop suspended domains and duplicate URIs (order preserved),
    // then cap the persisted page.
    let blocked: HashSet<String> = moderation::blocked_domains(&state.db, state.config().as_ref())
        .await?
        .into_iter()
        .collect();
    let mut seen = HashSet::new();
    let uris: Vec<String> = uris
        .into_iter()
        .filter(|u| {
            moderation::actor_domain(u)
                .map(|d| !blocked.contains(&d))
                .unwrap_or(true)
        })
        .filter(|u| seen.insert(u.clone()))
        .take(REMOTE_NETWORK_CAP)
        .collect();

    // 4. Resolve member URIs to `accounts.id`s. Known rows map in one
    // query; the first few unknowns fetch their actor document inline
    // (bounded concurrency) so the first page renders real profiles,
    // the rest land as stubs with a `RemoteActorFetch` enrichment job.
    let rows = sqlx::query(
        "SELECT id, actor_id FROM accounts WHERE actor_id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(&uris)
    .fetch_all(&state.db)
    .await?;
    let mut ids: HashMap<String, i64> = rows
        .iter()
        .map(|r| (r.get::<String, _>("actor_id"), r.get::<i64, _>("id")))
        .collect();
    let missing: Vec<String> = uris
        .iter()
        .filter(|u| !ids.contains_key(u.as_str()))
        .cloned()
        .collect();
    let (fetch_now, stub_later) = missing.split_at(missing.len().min(REMOTE_NETWORK_SYNC_FETCH));

    let handler = ap.multi.instance.clone();
    let fetched: Vec<(String, i64)> = futures_util::stream::iter(fetch_now.to_vec())
        .map(|uri| {
            let db = state.db.clone();
            let dispatcher = state.dispatcher.clone();
            let handler = handler.clone();
            async move {
                let u = uri.clone();
                let doc =
                    tokio::task::spawn_blocking(move || handler.outbox.fetch_actor(&u)).await;
                let id = match doc {
                    Ok(Some(doc)) => {
                        match crate::activitypub::storage::upsert_remote_actor_row(&db, &uri, &doc)
                            .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                tracing::warn!(actor = %uri, error = %e, "remote member upsert failed");
                                None
                            }
                        }
                    }
                    _ => None,
                };
                match id {
                    Some(id) => Some((uri, id)),
                    // Still list the member — a bare row renders
                    // username+domain and the job enriches it.
                    None => match lookup::upsert_remote_stub(&db, &dispatcher, &uri).await {
                        Ok(id) => Some((uri, id)),
                        Err(e) => {
                            tracing::warn!(actor = %uri, error = %e, "remote member stub failed");
                            None
                        }
                    },
                }
            }
        })
        .buffer_unordered(REMOTE_NETWORK_FETCH_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    ids.extend(fetched);
    for uri in stub_later {
        match lookup::upsert_remote_stub(&state.db, &state.dispatcher, uri).await {
            Ok(id) => {
                ids.insert(uri.clone(), id);
            }
            Err(e) => {
                tracing::warn!(actor = %uri, error = %e, "remote member stub failed");
            }
        }
    }

    // 5. Local `follows` edges the remote list didn't cover — e.g. a
    // just-landed local follower the remote collection omits, or
    // inbound edges when the remote hides its `following` collection.
    // They append after the remote members.
    let (member_col, filter_col) = match direction {
        "followers" => ("account_id", "target_account_id"),
        _ => ("target_account_id", "account_id"),
    };
    let extras: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT a.id FROM follows f \
         JOIN accounts a ON a.id = f.{member_col} \
         WHERE f.{filter_col} = $1 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
           AND NOT (a.actor_id = ANY($2)) \
         ORDER BY f.id DESC LIMIT $3",
    ))
    .bind(account_id)
    .bind(&uris)
    .bind(REMOTE_NETWORK_CAP as i64)
    .fetch_all(&state.db)
    .await?;

    // 6. Replace the stored edge list atomically — remote order first,
    // then the local extras; the claim row was stamped up front.
    let mut edges: Vec<(i64, i32)> = Vec::with_capacity(uris.len() + extras.len());
    let mut members = HashSet::new();
    for uri in &uris {
        if let Some(&id) = ids.get(uri) {
            if members.insert(id) {
                edges.push((id, edges.len() as i32));
            }
        }
    }
    for id in extras {
        if members.insert(id) {
            edges.push((id, edges.len() as i32));
        }
    }
    let member_ids: Vec<i64> = edges.iter().map(|e| e.0).collect();
    let positions: Vec<i32> = edges.iter().map(|e| e.1).collect();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM remote_network_edges WHERE account_id = $1 AND direction = $2")
        .bind(account_id)
        .bind(direction)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO remote_network_edges \
             (account_id, direction, member_account_id, position) \
         SELECT $1, $2, m_id, pos \
         FROM unnest($3::bigint[], $4::int[]) AS u(m_id, pos)",
    )
    .bind(account_id)
    .bind(direction)
    .bind(&member_ids)
    .bind(&positions)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `(account_id, cursor)` page over a remote account's cached network
/// edges — remote order, the cursor is `position` (`max_id` pages
/// older, `min_id`/`since_id` newer, same convention as `follows`).
pub async fn remote_edge_page(
    db: &PgPool,
    account_id: i64,
    direction: &str,
    page: &Page,
) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query(
        "SELECT member_account_id, position::bigint AS position FROM remote_network_edges \
         WHERE account_id = $1 AND direction = $2 \
           AND ($3::bigint IS NULL OR position > $3) \
           AND ($4::bigint IS NULL OR position < $4) \
         ORDER BY position LIMIT $5",
    )
    .bind(account_id)
    .bind(direction)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("member_account_id"), r.get("position")))
        .collect())
}
