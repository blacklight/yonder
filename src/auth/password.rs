//! Argon2id password hashing — the only KDF used for low-entropy user
//! secrets. High-entropy random material (tokens, client secrets) is
//! stored sha256-hex instead (see `crate::auth::tokens`).

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

use crate::Result;

/// Minimum accepted password length at registration.
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// Hash a plaintext password as an argon2id PHC string (`$argon2id$…`).
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| crate::Error::Internal(format!("password hashing failed: {e}")))?
        .to_string())
}

/// Verify a plaintext password against a stored PHC string.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// `POST /api/v1/yonder/account/password` — re-authenticate with the
/// current password, then rotate `users.password_hash` and revoke
/// every other access token (with its refresh tokens and session
/// rows). The caller's own token survives so the active session isn't
/// dropped — the Mastodon web behaviour.
///
/// Runs in one transaction so a mid-flight failure can't leave a new
/// hash paired with live stale sessions.
pub async fn change_password(
    pool: &sqlx::PgPool,
    user_id: i64,
    current_password: &str,
    new_password: &str,
    keep_token_id: i64,
) -> crate::Result<()> {
    if new_password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(crate::Error::Validation(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    let Some(hash) = stored else {
        return Err(crate::Error::Forbidden("incorrect password"));
    };
    if !verify_password(current_password, &hash) {
        return Err(crate::Error::Forbidden("incorrect password"));
    }
    let new_hash = hash_password(new_password)?;

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(user_id)
        .bind(&new_hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = now() \
         WHERE revoked_at IS NULL AND access_token_id IN ( \
             SELECT id FROM oauth_access_tokens \
             WHERE user_id = $1 AND id <> $2)",
    )
    .bind(user_id)
    .bind(keep_token_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE oauth_access_tokens SET revoked_at = now() \
         WHERE user_id = $1 AND id <> $2 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(keep_token_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND access_token_id <> $2")
        .bind(user_id)
        .bind(keep_token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
