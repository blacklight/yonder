//! Invite links (epic 11): creation gated by `auth.invite_links_enabled`,
//! atomic consumption inside the registration transaction, and the
//! public validity check the sign-up UI uses under closed registration.

use serde_json::{json, Value};
use sqlx::Postgres;

use crate::{
    activitypub, auth::tokens::random_secret, config::Config, http::AppState, Error, Result,
};

/// Whether `role` may create invite links: admins and moderators
/// always may; regular users only when the effective
/// `auth.invite_links_enabled` is true.
pub fn may_create(config: &Config, role: &str) -> bool {
    matches!(role, "admin" | "moderator") || config.auth.invite_links_enabled()
}

/// The invite's public URL — the frontend's `/invite/{code}` route.
fn invite_url(state: &AppState, code: &str) -> String {
    format!(
        "{}/invite/{}",
        activitypub::federation_base_url(&state.config()),
        code
    )
}

/// Render an `invites` row as the API entity.
fn entity(state: &AppState, row: &InviteRow) -> Value {
    json!({
        "id": row.id.to_string(),
        "code": row.code,
        "url": invite_url(state, &row.code),
        "uses": row.uses,
        "max_uses": row.max_uses,
        "expires_at": row.expires_at.map(|t| t.to_rfc3339()),
        "created_at": row.created_at.to_rfc3339(),
    })
}

/// An `invites` row.
pub struct InviteRow {
    /// Primary key.
    pub id: i64,
    /// The shareable code.
    pub code: String,
    /// Consumed uses so far.
    pub uses: i32,
    /// Optional use cap.
    pub max_uses: Option<i32>,
    /// Optional expiry (past = expired; revocation sets `now()`).
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Creation time.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Create an invite for `account_id`. `max_uses`/`expires_at` are
/// optional; a past `expires_at` is rejected.
pub async fn create(
    state: &AppState,
    account_id: i64,
    max_uses: Option<i32>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Value> {
    if let Some(max) = max_uses {
        if max < 1 {
            return Err(Error::Validation("max_uses must be at least 1".into()));
        }
    }
    if let Some(at) = expires_at {
        if at <= chrono::Utc::now() {
            return Err(Error::Validation("expires_at must be in the future".into()));
        }
    }
    // Retry on the (vanishingly unlikely) unique-code collision.
    for _ in 0..5 {
        let code = random_secret();
        let row = sqlx::query_as::<
            _,
            (
                i64,
                String,
                i32,
                Option<i32>,
                Option<chrono::DateTime<chrono::Utc>>,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "INSERT INTO invites (code, created_by_account_id, max_uses, expires_at) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, code, uses, max_uses, expires_at, created_at",
        )
        .bind(&code)
        .bind(account_id)
        .bind(max_uses)
        .bind(expires_at)
        .fetch_one(&state.db)
        .await;
        match row {
            Ok(r) => {
                let row = InviteRow {
                    id: r.0,
                    code: r.1,
                    uses: r.2,
                    max_uses: r.3,
                    expires_at: r.4,
                    created_at: r.5,
                };
                return Ok(entity(state, &row));
            }
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Internal("invite code allocation failed".into()))
}

/// List the caller's own invites, newest first.
pub async fn list(state: &AppState, account_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            String,
            i32,
            Option<i32>,
            Option<chrono::DateTime<chrono::Utc>>,
            chrono::DateTime<chrono::Utc>,
        ),
    >(
        "SELECT id, code, uses, max_uses, expires_at, created_at \
         FROM invites WHERE created_by_account_id = $1 ORDER BY created_at DESC",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            entity(
                state,
                &InviteRow {
                    id: r.0,
                    code: r.1,
                    uses: r.2,
                    max_uses: r.3,
                    expires_at: r.4,
                    created_at: r.5,
                },
            )
        })
        .collect())
}

/// Revoke an invite: sets `expires_at = now()` so the `users.invite_id`
/// audit trail survives. Owner or staff only (the caller enforces).
pub async fn revoke(
    state: &AppState,
    account_id: i64,
    is_staff: bool,
    invite_id: i64,
) -> Result<()> {
    let done = sqlx::query(
        "UPDATE invites SET expires_at = now() WHERE id = $1 \
         AND ($2 OR created_by_account_id = $3)",
    )
    .bind(invite_id)
    .bind(is_staff)
    .bind(account_id)
    .execute(&state.db)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(Error::NotFound("invite".into()));
    }
    Ok(())
}

/// Public validity check — a code is valid when it exists, hasn't
/// expired and hasn't exhausted `max_uses`.
pub async fn check(state: &AppState, code: &str) -> Result<Value> {
    let valid: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM invites WHERE code = $1 \
         AND (expires_at IS NULL OR expires_at > now()) \
         AND (max_uses IS NULL OR uses < max_uses))",
    )
    .bind(code)
    .fetch_one(&state.db)
    .await?;
    Ok(json!({ "valid": valid }))
}

/// Consume one invite use inside the registration transaction — a
/// single atomic `UPDATE … RETURNING` so concurrent sign-ups can never
/// exceed `max_uses`. Returns the invite id on success, `None` when
/// the code is unknown, expired or exhausted.
pub async fn consume(tx: &mut sqlx::Transaction<'_, Postgres>, code: &str) -> Result<Option<i64>> {
    let id: Option<i64> = sqlx::query_scalar(
        "UPDATE invites SET uses = uses + 1 WHERE code = $1 \
         AND (expires_at IS NULL OR expires_at > now()) \
         AND (max_uses IS NULL OR uses < max_uses) \
         RETURNING id",
    )
    .bind(code.trim())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(id)
}
