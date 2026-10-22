//! `/api/v1/sessions` — active interactive logins (browser/IP/activity).

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{
    auth::sessions,
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// `GET /api/v1/sessions` — the caller's sessions, newest first;
/// `current` marks the session this request rides on.
#[utoipa::path(
    get,
    path = "/api/v1/sessions",
    tag = "sessions",
    responses(
        (status = 200, description = "Active sessions"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    Ok(Json(Value::Array(
        sessions::list_sessions(&state.db, auth.user_id, auth.session_id).await?,
    )))
}

/// `DELETE /api/v1/sessions/{id}` — revoke the session and its linked
/// access token.
#[utoipa::path(
    delete,
    path = "/api/v1/sessions/{id}",
    tag = "sessions",
    params(("id" = i64, Path, description = "Session id")),
    responses(
        (status = 200, description = "Session revoked"),
        (status = 404, description = "Session not found", body = crate::error::ErrorBody),
    )
)]
pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Value>> {
    if !sessions::revoke_session(&state.db, auth.user_id, id).await? {
        return Err(Error::NotFound("session".into()));
    }
    Ok(Json(json!({})))
}
