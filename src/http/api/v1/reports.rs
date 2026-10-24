//! `/api/v1/reports` (user filing) and `/api/v1/admin/reports*` (the
//! moderator queue) — epic 21. Filing needs any authenticated user;
//! the admin surface goes through `require_moderator_scope` so both
//! admins and moderators see it.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::{require_moderator_scope, require_scope},
    http::{middleware::auth::AuthContext, AppState},
    moderation::reports,
    Error, Result,
};

/// `POST /api/v1/reports` — file a report against an account, with
/// optional status targets (each must belong to it), a comment capped
/// at `content.max_report_comment`, a category, and `forward` (send a
/// `Flag` to the target's instance when it is remote).
#[utoipa::path(
    post,
    path = "/api/v1/reports",
    tag = "reports",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "The filed Report"),
        (status = 401, description = "Not authenticated", body = crate::error::ErrorBody),
        (status = 403, description = "Scope rejected", body = crate::error::ErrorBody),
        (status = 404, description = "Target account or status", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid input", body = crate::error::ErrorBody),
    )
)]
pub async fn file(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<FileReport>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write")?;
    let comment = body.comment.unwrap_or_default();
    // The DB CHECK caps comments at 8000 — clamp the configured cap
    // so an over-large `max_report_comment` can't turn validation
    // into a database error.
    let max = std::cmp::min(
        state.config().content.max_report_comment as usize,
        reports::COMMENT_MAX_CHARS,
    );
    if comment.chars().count() > max {
        return Err(Error::Validation(format!(
            "comment exceeds {max} characters"
        )));
    }
    let target: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE id = $1 AND deleted_at IS NULL AND suspended_at IS NULL",
    )
    .bind(body.account_id)
    .fetch_optional(&state.db)
    .await?;
    let target = target.ok_or_else(|| Error::NotFound("account".into()))?;

    let status_ids = body.status_ids.unwrap_or_default();
    let category = body.category.as_deref().unwrap_or("other");
    let forward = body.forward.unwrap_or(false);
    let mut tx = state.db.begin().await?;
    let report = reports::file(
        &mut tx,
        auth.account_id,
        target,
        &status_ids,
        &comment,
        category,
        forward,
    )
    .await?;
    tx.commit().await?;
    // Best-effort — a fan-out failure must not drop the stored report.
    if let Err(e) =
        crate::notifications::notify_report(&state.db, &state.dispatcher, report.id).await
    {
        tracing::warn!(error = %e, report_id = report.id, "admin.report fan-out failed");
    }
    if forward {
        forward_flag(&state, &report, &status_ids).await;
    }
    Ok(Json(reports::entity(&state, &report).await?))
}

#[derive(Deserialize)]
pub struct FileReport {
    pub account_id: i64,
    #[serde(default)]
    pub status_ids: Option<Vec<i64>>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub forward: Option<bool>,
}

/// Forward the report as a `Flag` to the target's instance. Only
/// when the target is remote and federation is up — local targets
/// have nowhere to forward to. The activity is attributed to and
/// signed by the *instance actor*: the remote learns the instance
/// reported its user, not which local user filed it. `status_ids`
/// carries every reported status (one `reports` row each) so the
/// `Flag`'s `object` lists them all. Best-effort: a publish failure
/// is logged, the report is already stored.
async fn forward_flag(state: &Arc<AppState>, report: &reports::ReportRow, status_ids: &[i64]) {
    let Some(ap) = state.ap.as_ref() else {
        return;
    };
    let target_actor: Option<String> =
        sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NOT NULL")
            .bind(report.target_account_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    let Some(target_actor) = target_actor else {
        return;
    };
    let actor_id = crate::activitypub::actors::actor_id(
        &ap.core.base_url,
        crate::activitypub::actors::INSTANCE_USERNAME,
    );
    let mut objects = vec![target_actor.clone()];
    if !status_ids.is_empty() {
        objects.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT uri FROM statuses WHERE id = ANY($1) AND uri IS NOT NULL",
            )
            .bind(status_ids)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default(),
        );
    }
    let flag = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor_id}#flags/{}", report.id),
        "type": "Flag",
        "actor": actor_id,
        "object": objects,
        "content": report.comment,
        "to": [target_actor],
    });
    if let Err(e) = crate::activitypub::delivery::publish(
        ap,
        crate::activitypub::actors::INSTANCE_USERNAME,
        &flag,
    )
    .await
    {
        tracing::warn!(error = %e, report_id = report.id, "report forwarding (Flag) failed");
    }
}

/// `GET /api/v1/admin/reports` — the open queue by default;
/// `?resolved=true` flips it. `account_id`/`target_account_id` filter;
/// `max_id`/`since_id`/`limit` keyset-paginate.
#[utoipa::path(
    get,
    path = "/api/v1/admin/reports",
    tag = "admin",
    responses(
        (status = 200, description = "Array of Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(q): Query<ListParams>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:read")?;
    let rows = reports::list(
        &state.db,
        q.resolved,
        q.account_id,
        q.target_account_id,
        q.max_id,
        q.since_id,
        q.limit.unwrap_or(40).clamp(1, 200),
    )
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(reports::entity(&state, r).await?);
    }
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub resolved: Option<bool>,
    #[serde(default)]
    pub account_id: Option<i64>,
    #[serde(default)]
    pub target_account_id: Option<i64>,
    #[serde(default)]
    pub max_id: Option<i64>,
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/v1/admin/reports/{id}`.
#[utoipa::path(
    get,
    path = "/api/v1/admin/reports/{id}",
    tag = "admin",
    responses(
        (status = 200, description = "The Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such report", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:read")?;
    let report = reports::get(&state.db, id).await?;
    Ok(Json(reports::entity(&state, &report).await?))
}

/// `POST /api/v1/admin/reports/{id}/resolve` — mark handled; the
/// caller becomes the assignee when none is set. Audited.
#[utoipa::path(
    post,
    path = "/api/v1/admin/reports/{id}/resolve",
    tag = "admin",
    responses(
        (status = 200, description = "The resolved Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such report", body = crate::error::ErrorBody),
        (status = 409, description = "Already resolved", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_resolve(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    let mut tx = state.db.begin().await?;
    let report = reports::resolve(&mut tx, id, auth.account_id).await?;
    crate::moderation::audit::record(
        &mut *tx,
        Some(auth.account_id),
        "report.resolve",
        "report",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(reports::entity(&state, &report).await?))
}

/// `POST /api/v1/admin/reports/{id}/reopen` — clear resolution.
/// Audited.
#[utoipa::path(
    post,
    path = "/api/v1/admin/reports/{id}/reopen",
    tag = "admin",
    responses(
        (status = 200, description = "The reopened Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such report", body = crate::error::ErrorBody),
        (status = 409, description = "Not resolved", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_reopen(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    let mut tx = state.db.begin().await?;
    let report = reports::reopen(&mut tx, id).await?;
    crate::moderation::audit::record(
        &mut *tx,
        Some(auth.account_id),
        "report.reopen",
        "report",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(reports::entity(&state, &report).await?))
}

/// `POST /api/v1/admin/reports/{id}/assign_to_self` and
/// `POST /api/v1/admin/reports/{id}/unassign`. Audited.
#[utoipa::path(
    post,
    path = "/api/v1/admin/reports/{id}/assign_to_self",
    tag = "admin",
    responses(
        (status = 200, description = "The Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such report", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_assign_self(
    state: State<Arc<AppState>>,
    auth: AuthContext,
    path: Path<i64>,
) -> Result<Json<Value>> {
    assign_impl(state, auth, path, true).await
}

/// `POST /api/v1/admin/reports/{id}/unassign`.
#[utoipa::path(
    post,
    path = "/api/v1/admin/reports/{id}/unassign",
    tag = "admin",
    responses(
        (status = 200, description = "The Admin::Report"),
        (status = 403, description = "Moderator role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such report", body = crate::error::ErrorBody),
    )
)]
pub async fn admin_unassign(
    state: State<Arc<AppState>>,
    auth: AuthContext,
    path: Path<i64>,
) -> Result<Json<Value>> {
    assign_impl(state, auth, path, false).await
}

async fn assign_impl(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    self_assign: bool,
) -> Result<Json<Value>> {
    require_moderator_scope(&state.config(), &auth, "admin:write")?;
    let assignee = if self_assign {
        Some(auth.account_id)
    } else {
        None
    };
    let mut tx = state.db.begin().await?;
    let report = reports::assign(&mut tx, id, assignee).await?;
    crate::moderation::audit::record(
        &mut *tx,
        Some(auth.account_id),
        if self_assign {
            "report.assign_to_self"
        } else {
            "report.unassign"
        },
        "report",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(reports::entity(&state, &report).await?))
}
