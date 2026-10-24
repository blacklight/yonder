//! `GET /api/v1/admin/audit_log` — the moderation audit-log reader
//! (epic 22). Yonder keeps the name the BOOTSTRAP spec mandates;
//! Mastodon's equivalent surface is `/api/v1/admin/action_logs`.
//! Filtering and pagination live in [`crate::admin::audit`]; the
//! handler only authorizes, parses and paginates.

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, Uri},
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::{
    admin::audit::{self, Filters},
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, paginate, AppState},
    Error, Result,
};

/// `GET /api/v1/admin/audit_log` — admin-only. Filters: `action`
/// (prefix), `actor_account_id`, `target_type`, `target_id`, `since`,
/// `until` (RFC 3339); `max_id`/`min_id`/`limit` keyset-paginate and
/// the `Link` header carries cursors.
#[utoipa::path(
    get,
    path = "/api/v1/admin/audit_log",
    tag = "admin",
    params(
        ("action" = Option<String>, Query, description = "Action prefix, e.g. `report.`"),
        ("actor_account_id" = Option<i64>, Query, description = "Restrict to one actor"),
        ("target_type" = Option<String>, Query, description = "Restrict to one target kind"),
        ("target_id" = Option<i64>, Query, description = "Restrict to one target row"),
        ("since" = Option<String>, Query, description = "RFC 3339 lower bound on created_at"),
        ("until" = Option<String>, Query, description = "RFC 3339 upper bound on created_at"),
        ("max_id" = Option<String>, Query, description = "Rows older than this id"),
        ("min_id" = Option<String>, Query, description = "Rows newer than this id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)"),
    ),
    responses(
        (status = 200, description = "Audit entries, newest first"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid since/until", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "admin")?;
    let page = paginate::parse(&params);
    // A malformed filter is an error, never silently dropped — dropping
    // it would widen the result set.
    let int = |key: &str| -> Result<Option<i64>> {
        params
            .get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| Error::Validation(format!("invalid {key} {v:?}")))
            })
            .transpose()
    };
    let ts = |key: &str| -> Result<Option<DateTime<Utc>>> {
        params
            .get(key)
            .map(|v| {
                DateTime::parse_from_rfc3339(v)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|_| Error::Validation(format!("invalid {key} timestamp {v:?}")))
            })
            .transpose()
    };
    let filters = Filters {
        action: params.get("action").cloned(),
        actor_account_id: int("actor_account_id")?,
        target_type: params.get("target_type").cloned(),
        target_id: int("target_id")?,
        since: ts("since")?,
        until: ts("until")?,
    };
    let rows = audit::list(&state.db, &filters, &page).await?;
    let cursors: Vec<i64> = rows
        .iter()
        .filter_map(|row| row["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(rows))))
}
