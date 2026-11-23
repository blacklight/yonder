//! `/ap/users/{username}/collections/upcoming_events` — the FEP-8a8e
//! upcoming-events OrderedCollection (§16). Pubby has no route for
//! this path; the custom handler is registered before the
//! `dispatch_multi` catch-all like [`super::featured`].
//!
//! Scoping matches `featured`: only `public`/`unlisted` events render
//! (the collection is served through the shared doc-cache, so it
//! cannot vary per signer). Postponed events are excluded (no
//! `startTime` to order by) and the ordering is ascending by
//! `start_time` — "what's coming up", not a profile archive.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::http::AppState;
use crate::posts::{self, PostContext};
use crate::Error;

use super::actors::sync_block;

/// The doc-cache key — same shape pubby uses for collection routes.
pub fn upcoming_key(username: &str) -> pubby_sdk::cache::CacheKey {
    pubby_sdk::cache::route_key(&["collection", username, "/collections/upcoming_events"])
}

/// `GET /ap/users/{username}/collections/upcoming_events`.
pub async fn serve_upcoming_events(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let anonymous = super::edge::anonymous(&headers);
    match serve_upcoming_inner(&state, &username, &headers, &uri).await {
        Ok(doc) => super::edge::apply(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/activity+json")],
                Json(doc),
            )
                .into_response(),
            true,
            anonymous,
        ),
        Err(Error::Unauthorized) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => {
            // Only genuine misses are cacheable 404s — a transient
            // failure must not be frozen at the edge as "not found"
            // (same split as `serve_event_collection`).
            let not_found = matches!(e, Error::NotFound(_));
            if !not_found {
                tracing::warn!(username, error = %e, "upcoming_events collection failed");
            }
            super::edge::apply(
                (if not_found {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                })
                .into_response(),
                not_found,
                anonymous,
            )
        }
    }
}

async fn serve_upcoming_inner(
    state: &Arc<AppState>,
    username: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> crate::Result<Value> {
    let Some(ap) = &state.ap else {
        return Err(Error::NotFound("ap".into()));
    };
    let account = sqlx::query(
        "SELECT id, actor_id, username FROM accounts \
         WHERE lower(username) = lower($1) AND domain IS NULL AND deleted_at IS NULL",
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;
    let account_id: i64 = account.get("id");
    let actor_id: String = account.get("actor_id");
    // Cache under the canonical username — case-variant paths must
    // share one key so invalidation hits.
    let username = account.get::<String, _>("username");

    let signer = super::posts::verify_signature(state, ap, &username, headers, uri).await;
    if state.config().federation.authorized_fetch && signer.is_none() {
        return Err(Error::Unauthorized);
    }
    // Same gate as `serve_object`/featured: a signer blocked by (or
    // blocking / domain-blocking) this author gets a 404.
    if let Some(actor) = &signer {
        if let Some(sid) = super::posts::signer_account_id(&PostContext::new(state), actor).await? {
            if crate::social::visibility::author_hidden(&state.db, sid, account_id).await? {
                return Err(Error::NotFound("upcoming_events".into()));
            }
        }
    }

    let ctx = PostContext::new(state);
    let doc = ap
        .core
        .doc_cache
        .get_or_render(
            upcoming_key(&username),
            move || {
                let ctx = ctx.clone();
                sync_block(async move {
                    build_collection(&ctx, account_id, &actor_id)
                        .await
                        .map(Some)
                        .map_err(|e| pubby_sdk::ActivityPubError::General(e.to_string()))
                })
            },
            None,
            None,
        )
        .map_err(|e| Error::Internal(e.to_string()))?;
    doc.ok_or_else(|| Error::NotFound("upcoming_events".into()))
}

/// The OrderedCollection document: the author's upcoming local events
/// (public/unlisted, not tombstoned/postponed, `end_time`/`start_time`
/// still in the future) ascending by `start_time`.
async fn build_collection(
    ctx: &PostContext,
    account_id: i64,
    actor_id: &str,
) -> crate::Result<Value> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM statuses s JOIN events e ON e.status_id = s.id \
         WHERE s.account_id = $1 AND s.object_type = 'Event' AND NOT s.tombstone \
           AND e.start_time IS NOT NULL AND e.status <> 'postponed' \
           AND COALESCE(e.end_time, e.start_time) >= now() \
           AND s.visibility IN ('public', 'unlisted') \
         ORDER BY e.start_time ASC \
         LIMIT 200",
    )
    .bind(account_id)
    .fetch_all(&ctx.db)
    .await?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(status) = posts::load_any(&ctx.db, id).await? else {
            continue;
        };
        if status.tombstone {
            continue;
        }
        let mut doc = super::posts::build_object(ctx, &status).await?;
        doc["@context"] = super::context::event_context();
        items.push(doc);
    }
    Ok(json!({
        "@context": super::context::event_context(),
        "id": format!("{actor_id}/collections/upcoming_events"),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    }))
}

/// Invalidate the author's cached collection — called on event
/// create/edit/tombstone (best-effort; federation-less setups have no
/// cache to clear).
pub async fn invalidate(ctx: &PostContext, account_id: i64) {
    let username: Option<String> =
        sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1 AND domain IS NULL")
            .bind(account_id)
            .fetch_optional(&ctx.db)
            .await
            .ok()
            .flatten();
    if let (Some(username), Some(ap)) = (username, &ctx.ap) {
        ap.core.doc_cache.invalidate(&[upcoming_key(&username)]);
    }
}
