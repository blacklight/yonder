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
    notifications::{self, NotificationType},
    posts::PostContext,
    Error, Result,
};

/// Type-filter values that aren't Mastodon types but the Yonder API
/// accepts: `all` (no filter), and the group aliases from the epic
/// spec.
fn expand_filter(value: &str, out: &mut Vec<NotificationType>) -> Result<()> {
    const ALIASES: &[(&str, &[&str])] = &[
        ("likes", &["favourite"]),
        ("boosts", &["reblog"]),
        ("quotes", &["quote"]),
        ("mentions", &["mention"]),
        ("reports", &["admin.report"]),
    ];
    if value == "all" {
        return Ok(());
    }
    if let Some((_, names)) = ALIASES.iter().find(|(alias, _)| *alias == value) {
        for name in *names {
            out.push(NotificationType::from_wire(name).expect("alias maps to a known type"));
        }
        return Ok(());
    }
    match NotificationType::from_wire(value) {
        Some(kind) => {
            out.push(kind);
            Ok(())
        }
        None => Err(Error::Validation(format!(
            "unknown notification type: {value}"
        ))),
    }
}

/// Repeated `types[]` / `exclude_types[]` params from the raw query —
/// `Query<HashMap>` keeps only the last value.
fn type_filters(uri: &Uri) -> Result<(Vec<NotificationType>, Vec<NotificationType>)> {
    let mut types = Vec::new();
    let mut exclude = Vec::new();
    for (k, v) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        match k.as_ref() {
            "types" | "types[]" => expand_filter(&v, &mut types)?,
            "exclude_types" | "exclude_types[]" => expand_filter(&v, &mut exclude)?,
            _ => {}
        }
    }
    Ok((types, exclude))
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
/// `exclude_types[]` filter by Mastodon type or Yonder alias
/// (`all`, `likes`, `boosts`, `quotes`, `mentions`, `reports`).
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
        (status = 422, description = "Unknown type filter", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:notifications")?;
    let (types, exclude) = type_filters(&uri)?;
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
    let cursors: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    let ctx = PostContext::new(&state);
    let out = notifications::entities(&ctx, &rows, &rules).await?;
    Ok((headers, Json(Value::Array(out))))
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

/// Body for `POST /notifications/read` — ids the client rendered.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct MarkReadBody {
    /// Notification ids to mark read (max [`MAX_MARK_READ_IDS`]).
    pub ids: Vec<i64>,
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
    notifications::mark_read(&state.db, auth.account_id, &body.ids).await?;
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
