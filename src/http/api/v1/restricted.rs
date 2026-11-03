//! The restricted (suspended-account) surface (epic 31): the only
//! endpoints a `restricted`-class token may reach. Account-access
//! metadata and logout live here; export request/list/status/download
//! reuse the ordinary `transfer` handlers under `ExportAuth`.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    http::{middleware::auth::RestrictedAuth, AppState},
    Result,
};

/// `GET /api/v1/yonder/restricted/session` — account-access metadata
/// for the restricted page: the suspension marker, the account's own
/// email, and the *configured public* instance contact (never an
/// arbitrary admin address; `null` when unset).
#[utoipa::path(
    get,
    path = "/api/v1/yonder/restricted/session",
    tag = "restricted",
    responses(
        (status = 200, description = "Restricted session metadata"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Not a restricted session", body = crate::error::ErrorBody),
    )
)]
pub async fn restricted_session(
    State(state): State<Arc<AppState>>,
    auth: RestrictedAuth,
) -> Result<Json<Value>> {
    let row = sqlx::query(
        "SELECT u.email, a.username, a.suspended_at FROM users u \
         JOIN accounts a ON a.id = u.account_id WHERE u.id = $1",
    )
    .bind(auth.0.user_id)
    .fetch_one(&state.db)
    .await?;
    let config = &state.config();
    let base_url = crate::activitypub::federation_base_url(config);
    let contact_account = super::instance::contact_account(&state.db, config, &base_url).await;
    Ok(Json(json!({
        "restricted": true,
        "account": {
            "username": row.get::<String, _>("username"),
            "acct": row.get::<String, _>("username"),
        },
        "email": row.get::<Option<String>, _>("email"),
        "suspended_at": row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("suspended_at")
            .map(|t| t.to_rfc3339()),
        "contact_email": config.instance.contact_email,
        "contact_account": contact_account,
    })))
}

/// `POST /api/v1/yonder/restricted/logout` — revoke the restricted
/// token and drop its session row.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/restricted/logout",
    tag = "restricted",
    responses(
        (status = 200, description = "Session revoked"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Not a restricted session", body = crate::error::ErrorBody),
    )
)]
pub async fn restricted_logout(
    State(state): State<Arc<AppState>>,
    auth: RestrictedAuth,
) -> Result<Json<Value>> {
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE oauth_access_tokens SET revoked_at = now() WHERE id = $1")
        .bind(auth.0.token_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE access_token_id = $1")
        .bind(auth.0.token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true })))
}
