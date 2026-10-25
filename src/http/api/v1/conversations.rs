//! `/api/v1/conversations*` — Mastodon-compatible direct-message
//! conversations (epic 25).
//!
//! The path `{id}` is the conversation key itself (64 lowercase hex —
//! see `posts::conversations`). A well-formed key never grants access:
//! every handler verifies the caller is a participant first, and
//! non-participants and unknown keys get the same 404. Markers
//! (read/hidden) are written for the authenticated caller only.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{
        middleware::auth::AuthContext,
        paginate::{self, Page},
        AppState,
    },
    posts::{self, conversations as conv, PostContext},
    users::account as account_mapper,
    Error, Result,
};

/// `{id}` must be the 64-lowercase-hex conversation key. Anything else
/// is the same 404 as an unknown key — before any DB access.
fn parse_key(id: &str) -> Result<&str> {
    if id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(id)
    } else {
        Err(Error::NotFound("conversation".into()))
    }
}

/// Participant gate shared by every `{id}` handler.
async fn require_participant(state: &AppState, caller: i64, key: &str) -> Result<()> {
    if !conv::is_participant(&state.db, key, caller).await? {
        return Err(Error::NotFound("conversation".into()));
    }
    Ok(())
}

/// Everything `conversation_json` needs for a page, fetched in three
/// queries instead of three per conversation: the newest status rows,
/// the participant ids per key, and the rendered Account entities
/// keyed by account id.
struct PageData {
    statuses: HashMap<i64, posts::StatusRow>,
    participants: HashMap<String, Vec<i64>>,
    accounts: HashMap<i64, Value>,
}

async fn load_page_data(
    state: &AppState,
    caller: i64,
    groups: &[conv::ConversationGroup],
) -> Result<PageData> {
    let keys: Vec<String> = groups.iter().map(|g| g.key.clone()).collect();
    let newest: Vec<i64> = groups.iter().map(|g| g.newest_id).collect();
    let statuses = conv::statuses_by_id(&state.db, &newest).await?;
    let participants = conv::other_participants_many(&state.db, &keys, caller).await?;
    let mut all_ids: Vec<i64> = participants.values().flatten().copied().collect();
    all_ids.sort_unstable();
    all_ids.dedup();
    let rows = account_mapper::load_many(&state.db, &all_ids).await?;
    let ctx = PostContext::new(state);
    let base = ctx.base_url();
    let entities =
        account_mapper::account_entities(&state.db, &state.config(), &base, &rows).await?;
    let accounts = entities
        .into_iter()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.parse::<i64>().ok()?;
            Some((id, v))
        })
        .collect();
    Ok(PageData {
        statuses,
        participants,
        accounts,
    })
}

/// Render one Conversation entity: `id` (the key), `unread`, the other
/// participants' Account entities, and `last_status` (newest visible —
/// tombstoned last messages fall back by construction).
async fn conversation_json(
    state: &AppState,
    caller: i64,
    group: &conv::ConversationGroup,
    marker: Option<&(Option<i64>, Option<i64>)>,
    data: &PageData,
) -> Result<Value> {
    let ctx = PostContext::new(state);
    let last = data.statuses.get(&group.newest_id);
    let last_status = match last {
        Some(row) => posts::entity::status_entity(&ctx, row, Some(caller)).await?,
        None => Value::Null,
    };
    let account_json: Vec<Value> = data
        .participants
        .get(&group.key)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| data.accounts.get(id).cloned())
                .collect()
        })
        .unwrap_or_default();
    let (read, hidden) = marker.copied().unwrap_or((None, None));
    let unread = last.is_some_and(|s| s.account_id != caller)
        && group.newest_id > read.unwrap_or(0)
        && group.newest_id > hidden.unwrap_or(0);
    Ok(json!({
        "id": group.key,
        "unread": unread,
        "accounts": account_json,
        "last_status": last_status,
    }))
}

/// `GET /api/v1/conversations` — the caller's DM conversations,
/// newest-activity first, `Link`-paginated by the group's newest
/// visible status id.
#[utoipa::path(
    get,
    path = "/api/v1/conversations",
    tag = "conversations",
    params(
        ("max_id" = Option<String>, Query, description = "Return conversations older than this cursor"),
        ("min_id" = Option<String>, Query, description = "Return conversations immediately newer than this cursor"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of conversations (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn list_conversations(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let page = paginate::parse(&params);
    let groups = conv::list(&state.db, auth.account_id, &page).await?;
    let keys: Vec<String> = groups.iter().map(|g| g.key.clone()).collect();
    let markers = conv::markers(&state.db, auth.account_id, &keys).await?;
    let data = load_page_data(&state, auth.account_id, &groups).await?;
    let mut out = Vec::with_capacity(groups.len());
    for g in &groups {
        out.push(conversation_json(&state, auth.account_id, g, markers.get(&g.key), &data).await?);
    }
    let cursors: Vec<i64> = groups.iter().map(|g| g.newest_id).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(out))))
}

/// `GET /api/v1/conversations/{id}/statuses` — the thread, newest
/// first, `Link`-paginated like the timelines.
#[utoipa::path(
    get,
    path = "/api/v1/conversations/{id}/statuses",
    tag = "conversations",
    params(
        ("id" = String, Path, description = "Conversation id"),
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of statuses, newest first (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown conversation or not a participant", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn conversation_statuses(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let key = parse_key(&id)?;
    require_participant(&state, auth.account_id, key).await?;
    let page = paginate::parse(&params);
    let rows = conv::thread(&state.db, auth.account_id, key, &page).await?;
    let ctx = PostContext::new(&state);
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(posts::entity::status_entity(&ctx, row, Some(auth.account_id)).await?);
    }
    let cursors = crate::timelines::query::cursors(&rows);
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(out))))
}

/// `POST /api/v1/conversations/{id}/read` — set the caller's read
/// marker to the newest visible status; returns the conversation.
#[utoipa::path(
    post,
    path = "/api/v1/conversations/{id}/read",
    tag = "conversations",
    params(("id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "The conversation"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown conversation or not a participant", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn mark_conversation_read(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:conversations")?;
    let key = parse_key(&id)?;
    require_participant(&state, auth.account_id, key).await?;
    conv::mark_read(&state.db, auth.account_id, key).await?;
    // Newest visible status id — the group's cursor (the participant
    // gate already ran above).
    let one = Page {
        max_id: None,
        since_id: None,
        limit: 1,
    };
    let newest_id = conv::thread(&state.db, auth.account_id, key, &one)
        .await?
        .first()
        .map(|s| s.id)
        .unwrap_or(0);
    let group = conv::ConversationGroup {
        key: key.to_string(),
        newest_id,
    };
    let marker = conv::markers(&state.db, auth.account_id, &[key.to_string()]).await?;
    let data = load_page_data(&state, auth.account_id, std::slice::from_ref(&group)).await?;
    Ok(Json(
        conversation_json(&state, auth.account_id, &group, marker.get(key), &data).await?,
    ))
}

/// `DELETE /api/v1/conversations/{id}` — caller-only hide: sets
/// `hidden_through_status_id` to the newest status id with the key.
/// Statuses are never deleted; a newer status revives the
/// conversation for this caller.
#[utoipa::path(
    delete,
    path = "/api/v1/conversations/{id}",
    tag = "conversations",
    params(("id" = String, Path, description = "Conversation id")),
    responses(
        (status = 200, description = "Hidden for the caller"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown conversation or not a participant", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:conversations")?;
    let key = parse_key(&id)?;
    require_participant(&state, auth.account_id, key).await?;
    conv::hide(&state.db, auth.account_id, key).await?;
    Ok(Json(json!({})))
}
