//! `/api/v1` notification endpoints (epic 16) — list with type
//! filters, single get, clear, dismiss, and `unread_count`.
//!
//! Handlers stay thin: scope check → [`notifications`] service →
//! entity JSON. `types[]`/`exclude_types[]` are repeated params, which
//! `Query<HashMap>` can't represent — they're parsed from the raw
//! query string instead.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Uri},
    response::Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, paginate, AppState},
    notifications,
    posts::PostContext,
    Error, Result,
};

/// Expands one `types[]`/`exclude_types[]` value into wire type
/// names. Yonder group aliases (`likes`, `boosts`, …) expand to
/// several; `all` expands to nothing (no filter). Values that are no
/// type Yonder emits — newer Mastodon types clients still enumerate
/// (`severed_relationships`), or other implementations' names — pass
/// through verbatim, matching Mastodon: `types[]` then selects no
/// rows, `exclude_types[]` excludes nothing. Synonyms for types
/// Yonder does emit under a different name fold onto it (Misskey's
/// `reaction`, Akkoma's `pleroma:emoji_reaction` → `emoji_reaction`)
/// so those filters actually apply.
fn expand_filter(value: &str, out: &mut Vec<String>) {
    const ALIASES: &[(&str, &[&str])] = &[
        ("likes", &["favourite"]),
        ("boosts", &["reblog"]),
        ("quotes", &["quote"]),
        ("mentions", &["mention"]),
        ("reports", &["admin.report"]),
        ("reaction", &["emoji_reaction"]),
        ("reactions", &["emoji_reaction"]),
        ("pleroma:emoji_reaction", &["emoji_reaction"]),
    ];
    if value == "all" {
        return;
    }
    if let Some((_, names)) = ALIASES.iter().find(|(alias, _)| *alias == value) {
        out.extend(names.iter().map(|s| s.to_string()));
        return;
    }
    out.push(value.to_string());
}

/// Repeated `types[]` / `exclude_types[]` params from the raw query —
/// `Query<HashMap>` keeps only the last value.
fn type_filters(uri: &Uri) -> (Vec<String>, Vec<String>) {
    let mut types = Vec::new();
    let mut exclude = Vec::new();
    for (k, v) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        match k.as_ref() {
            "types" | "types[]" => expand_filter(&v, &mut types),
            "exclude_types" | "exclude_types[]" => expand_filter(&v, &mut exclude),
            _ => {}
        }
    }
    (types, exclude)
}

/// Publish the recipient's current unread count to the live
/// `user:notification` stream (Yonder extension event). Hidden rows
/// are excluded when the caller's rules load — a load failure just
/// counts them, since the number carries no filtered content.
async fn publish_count(state: &AppState, user_id: i64, account_id: i64) {
    let rules = crate::filters::rules::load_active(
        &state.db,
        account_id,
        crate::filters::FilterContext::Notifications,
    )
    .await
    .unwrap_or_default();
    let count = notifications::unread_count(
        &state.db,
        account_id,
        crate::filters::apply::hide_clause(&rules).as_ref(),
    )
    .await
    .unwrap_or(0);
    notifications::publish_live(
        &state.redis,
        &format!("user:notification:{user_id}"),
        "notifications_count",
        &json!({ "unread_count": count }),
    )
    .await;
}

/// `GET /api/v1/notifications` — the caller's notifications,
/// newest-first, `Link`-header paginated. `types[]` /
/// `exclude_types[]` filter by notification type or Yonder alias
/// (`all`, `likes`, `boosts`, `quotes`, `mentions`, `reports`,
/// `reaction`). Unrecognized names are accepted and match/exclude
/// nothing — Mastodon's semantics — so clients enumerating newer
/// types (`severed_relationships`, …) don't get 422'd.
#[utoipa::path(
    get,
    path = "/api/v1/notifications",
    tag = "notifications",
    params(
        ("types[]" = Option<String>, Query, description = "Repeatable type/alias filter"),
        ("exclude_types[]" = Option<String>, Query, description = "Repeatable type exclusion"),
        ("max_id" = Option<String>, Query, description = "Return notifications older than this id"),
        ("min_id" = Option<String>, Query, description = "Return notifications immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of notifications, newest first (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    let (types, exclude) = type_filters(&uri);
    let page = paginate::parse(&params);
    let rules = crate::filters::rules::load_active(
        &state.db,
        auth.account_id,
        crate::filters::FilterContext::Notifications,
    )
    .await?;
    let rows = notifications::list(
        &state.db,
        auth.account_id,
        &types,
        &exclude,
        &page,
        crate::filters::apply::hide_clause(&rules).as_ref(),
    )
    .await?;
    // Conversation mutes: a muted thread's notifications drop out of
    // the page (Mastodon hides them rather than never generating).
    let status_ids: Vec<i64> = rows.iter().filter_map(|r| r.status_id).collect();
    let muted =
        crate::interactions::mutes::muted_status_ids(&state.db, Some(auth.account_id), &status_ids)
            .await?;
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| r.status_id.is_none_or(|id| !muted.contains(&id)))
        .collect();
    let cursors: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    let ctx = PostContext::new(&state);
    let out = notifications::entities(&ctx, &rows, &rules).await?;
    Ok((headers, Json(Value::Array(out))))
}

/// `GET /api/v1/notifications/policy` — the Mastodon 4.3+
/// NotificationPolicy entity. Flags persist per account; until the
/// requests inbox lands they are informational only (`summary` is
/// always zero).
#[utoipa::path(
    get,
    path = "/api/v1/notifications/policy",
    tag = "notifications",
    responses(
        (status = 200, description = "The NotificationPolicy entity"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn get_policy(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    Ok(Json(policy_entity(&state.db, auth.account_id).await?))
}

async fn policy_entity(db: &sqlx::PgPool, account_id: i64) -> Result<Value> {
    let row = sqlx::query_as::<_, (bool, bool, bool, bool)>(
        "SELECT filter_not_followers, filter_not_following, \
                filter_new_accounts, filter_private_mentions \
         FROM notification_policies WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    let (nf, ng, na, pm) = row.unwrap_or_default();
    Ok(json!({
        "filter_not_followers": nf,
        "filter_not_following": ng,
        "filter_new_accounts": na,
        "filter_private_mentions": pm,
        "summary": {
            "pending_requests_count": 0,
            "pending_notifications_count": 0,
        },
    }))
}

/// `PUT /api/v1/notifications/policy` — update the flags (form or
/// JSON booleans). Missing keys keep their value.
#[utoipa::path(
    put,
    path = "/api/v1/notifications/policy",
    tag = "notifications",
    responses(
        (status = 200, description = "The updated NotificationPolicy"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn put_policy(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: axum::extract::Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    use axum::http::header;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    let mut updates: Vec<(String, bool)> = Vec::new();
    const FLAGS: &[&str] = &[
        "filter_not_followers",
        "filter_not_following",
        "filter_new_accounts",
        "filter_private_mentions",
    ];
    if content_type.starts_with("application/x-www-form-urlencoded") {
        for (k, v) in url::form_urlencoded::parse(&bytes) {
            if FLAGS.contains(&k.as_ref()) {
                updates.push((k.into_owned(), crate::users::settings::parse_bool_str(&v)?));
            }
        }
    } else if !bytes.is_empty() {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::BadRequest(format!("malformed JSON body: {e}")))?;
        for f in FLAGS {
            if let Some(b) = body.get(*f).and_then(|v| v.as_bool()) {
                updates.push(((*f).to_string(), b));
            }
        }
    }
    for (flag, value) in updates {
        // `flag` comes from a fixed allowlist, never user input.
        sqlx::query(&format!(
            "INSERT INTO notification_policies (account_id, {flag}) VALUES ($1,$2) \
             ON CONFLICT (account_id) DO UPDATE SET {flag} = EXCLUDED.{flag}, \
                 updated_at = now()"
        ))
        .bind(auth.account_id)
        .bind(value)
        .execute(&state.db)
        .await?;
    }
    Ok(Json(policy_entity(&state.db, auth.account_id).await?))
}

/// `GET /api/v1/notifications/requests` — the filtered-notification
/// requests inbox. Nothing lands there yet (policy flags are stored
/// but unapplied), so the page is always empty.
#[utoipa::path(
    get,
    path = "/api/v1/notifications/requests",
    tag = "notifications",
    responses((status = 200, description = "Empty page of NotificationRequest entities"))
)]
pub async fn requests(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    Ok(Json(json!([])))
}

/// `POST /api/v1/notifications/requests/{id}/accept` — 404: no
/// requests exist while the inbox is empty.
#[utoipa::path(
    post,
    path = "/api/v1/notifications/requests/{id}/accept",
    tag = "notifications",
    params(("id" = String, Path, description = "Request id")),
    responses(
        (status = 200, description = "Accepted"),
        (status = 404, description = "No such request", body = crate::error::ErrorBody),
    )
)]
pub async fn accept_request(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(_id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    Err(Error::NotFound("notification_request".into()))
}

/// `POST /api/v1/notifications/requests/{id}/dismiss` — 404.
#[utoipa::path(
    post,
    path = "/api/v1/notifications/requests/{id}/dismiss",
    tag = "notifications",
    params(("id" = String, Path, description = "Request id")),
    responses(
        (status = 200, description = "Dismissed"),
        (status = 404, description = "No such request", body = crate::error::ErrorBody),
    )
)]
pub async fn dismiss_request(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(_id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    Err(Error::NotFound("notification_request".into()))
}

/// `GET /api/v1/notifications/{id}` — one notification, owner-scoped.
#[utoipa::path(
    get,
    path = "/api/v1/notifications/{id}",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "The notification"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    let row = notifications::get(&state.db, auth.account_id, id)
        .await?
        .ok_or_else(|| Error::NotFound("notification".into()))?;
    let ctx = PostContext::new(&state);
    let rules = crate::filters::rules::load_active(
        &state.db,
        auth.account_id,
        crate::filters::FilterContext::Notifications,
    )
    .await?;
    if notifications::hidden(
        &state.db,
        &row,
        crate::filters::apply::hide_clause(&rules).as_ref(),
    )
    .await?
    {
        return Err(Error::NotFound("notification".into()));
    }
    Ok(Json(notifications::entity(&ctx, &row, &rules).await?))
}

/// `POST /api/v1/notifications/clear` — delete all of the caller's
/// notifications (200 on an empty list too).
#[utoipa::path(
    post,
    path = "/api/v1/notifications/clear",
    tag = "notifications",
    responses(
        (status = 200, description = "All notifications cleared"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
    )
)]
pub async fn clear(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    notifications::clear(&state.db, auth.account_id).await?;
    publish_count(&state, auth.user_id, auth.account_id).await;
    Ok(Json(json!({})))
}

/// A notification id in either wire form. Entities serialize ids as
/// strings — snowflake values exceed JS's `Number.MAX_SAFE_INTEGER`,
/// so round-tripping through a JS number silently corrupts them —
/// but bare JSON numbers are accepted for convenience.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum NotificationId {
    /// String form (`"1234"`), matching entity serialization.
    Str(String),
    /// Bare JSON number (`1234`).
    Num(i64),
}

/// Body for `POST /notifications/read` — ids the client rendered.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct MarkReadBody {
    /// Notification ids to mark read (max [`MAX_MARK_READ_IDS`]).
    pub ids: Vec<NotificationId>,
}

/// Upper bound on ids per `mark_read` call — the SPA batches viewport
/// hits far below this.
const MAX_MARK_READ_IDS: usize = 200;

/// `POST /api/v1/notifications/read` — mark the caller's rows read
/// (Yonder extension; the SPA drives it from the rendered viewport).
#[utoipa::path(
    post,
    path = "/api/v1/notifications/read",
    tag = "notifications",
    request_body = MarkReadBody,
    responses(
        (status = 200, description = "The ids were marked read"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 422, description = "Too many ids", body = crate::error::ErrorBody),
    )
)]
pub async fn mark_read(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<MarkReadBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    if body.ids.len() > MAX_MARK_READ_IDS {
        return Err(Error::Validation("too many notification ids".into()));
    }
    let mut ids = Vec::with_capacity(body.ids.len());
    for id in &body.ids {
        ids.push(match id {
            NotificationId::Num(v) => *v,
            NotificationId::Str(s) => s
                .parse()
                .map_err(|_| Error::Validation("invalid notification id".into()))?,
        });
    }
    notifications::mark_read(&state.db, auth.account_id, &ids).await?;
    publish_count(&state, auth.user_id, auth.account_id).await;
    Ok(Json(json!({})))
}

/// `POST /api/v1/notifications/{id}/dismiss` — delete one notification.
#[utoipa::path(
    post,
    path = "/api/v1/notifications/{id}/dismiss",
    tag = "notifications",
    params(("id" = String, Path, description = "Notification id")),
    responses(
        (status = 200, description = "The notification was dismissed"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn dismiss(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:notifications")?;
    if !notifications::dismiss(&state.db, auth.account_id, id).await? {
        return Err(Error::NotFound("notification".into()));
    }
    publish_count(&state, auth.user_id, auth.account_id).await;
    Ok(Json(json!({})))
}

/// `GET /api/v1/notifications/unread_count` — mirrors Mastodon's
/// `{count}` response shape; the badge count lives here.
#[utoipa::path(
    get,
    path = "/api/v1/notifications/unread_count",
    tag = "notifications",
    responses(
        (status = 200, description = "The unread notification count"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unread_count(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    let rules = crate::filters::rules::load_active(
        &state.db,
        auth.account_id,
        crate::filters::FilterContext::Notifications,
    )
    .await?;
    let count = notifications::unread_count(
        &state.db,
        auth.account_id,
        crate::filters::apply::hide_clause(&rules).as_ref(),
    )
    .await?;
    Ok(Json(json!({ "count": count })))
}
