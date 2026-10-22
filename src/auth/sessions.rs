//! Browser-session tracking: every interactive (password-grant) login
//! creates a `sessions` row linked to its access token. Listing shows
//! browser/IP/last-activity; revocation kills the linked token.

use serde_json::{json, Value};
use sqlx::Row;

use crate::Result;

/// `GET /api/v1/sessions` — the caller's sessions, newest activity
/// first. `current` marks the session this request rides on.
pub async fn list_sessions(
    pool: &sqlx::PgPool,
    user_id: i64,
    current_session: Option<i64>,
) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, user_agent, ip::text AS ip, last_activity_at, created_at \
         FROM sessions WHERE user_id = $1 ORDER BY last_activity_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let id: i64 = r.get("id");
            json!({
                "id": id.to_string(),
                "browser": r.get::<Option<String>, _>("user_agent"),
                "ip": r.get::<Option<String>, _>("ip"),
                "last_activity_at": r.get::<chrono::DateTime<chrono::Utc>, _>("last_activity_at").to_rfc3339(),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
                "current": Some(id) == current_session,
            })
        })
        .collect())
}

/// `DELETE /api/v1/sessions/{id}` — revoke the session's linked access
/// token (and its refresh tokens) then remove the session row.
pub async fn revoke_session(pool: &sqlx::PgPool, user_id: i64, session_id: i64) -> Result<bool> {
    let access_token_id: Option<Option<i64>> =
        sqlx::query_scalar("SELECT access_token_id FROM sessions WHERE id = $1 AND user_id = $2")
            .bind(session_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    let Some(Some(access_token_id)) = access_token_id else {
        return Ok(false);
    };
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE oauth_refresh_tokens SET revoked_at = now() WHERE access_token_id = $1")
        .bind(access_token_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE oauth_access_tokens SET revoked_at = now() WHERE id = $1")
        .bind(access_token_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2")
        .bind(session_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
