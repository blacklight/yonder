//! `/ap/users/{username}/collections/featured` — the pinned-posts
//! OrderedCollection (epic 13). Pubby serves an *empty* collection on
//! this path; this custom route replaces it (registered before the
//! `dispatch_multi` catch-all, like `serve_object`).
//!
//! Only `public`/`unlisted` pins render — private pins stay
//! API-visible but out of the public collection. Documents are
//! served through the shared doc-cache; [`pins`] invalidates it on
//! pin/unpin.

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
pub fn featured_key(username: &str) -> pubby_sdk::cache::CacheKey {
    pubby_sdk::cache::route_key(&["collection", username, "/collections/featured"])
}

/// `GET /ap/users/{username}/collections/featured`.
pub async fn serve_featured(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    match serve_featured_inner(&state, &username, &headers, &uri).await {
        Ok(doc) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/activity+json")],
            Json(doc),
        )
            .into_response(),
        Err(Error::Unauthorized) => StatusCode::UNAUTHORIZED.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn serve_featured_inner(
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
    // Cache under the canonical username — case-variant paths
    // (`/Alice` vs `/alice`) must share one key so invalidation hits.
    let username = account.get::<String, _>("username");

    if state.config.federation.authorized_fetch {
        let signer = super::posts::verify_signature(state, ap, &username, headers, uri).await;
        if signer.is_none() {
            return Err(Error::Unauthorized);
        }
    }

    let ctx = PostContext::new(state);
    let doc = ap
        .core
        .doc_cache
        .get_or_render(
            featured_key(&username),
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
    doc.ok_or_else(|| Error::NotFound("featured".into()))
}

/// The OrderedCollection document: pinned local statuses' object
/// docs ordered by `idx`, public/unlisted only.
async fn build_collection(
    ctx: &PostContext,
    account_id: i64,
    actor_id: &str,
) -> crate::Result<Value> {
    let ids = crate::interactions::pins::pinned_ids(ctx, account_id).await?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(status) = posts::load_any(&ctx.db, id).await? else {
            continue;
        };
        if !status.local
            || status.tombstone
            || !matches!(status.visibility.as_str(), "public" | "unlisted")
        {
            continue;
        }
        let mut doc = super::posts::build_object(ctx, &status).await?;
        doc["@context"] = pubby_sdk::model::ap_context();
        items.push(doc);
    }
    Ok(json!({
        "@context": pubby_sdk::model::ap_context(),
        "id": format!("{actor_id}/collections/featured"),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    }))
}
