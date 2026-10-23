//! Subscribe — the `subscriptions` table (BOOTSTRAP "Profile pages"):
//! get a notification when an account posts, *without* following it.
//! The Mastodon path for the same effect is `follows.notify`; both are
//! read by epic 16's `status` fanout.

use sqlx::PgPool;

use crate::Result;

/// Subscribe `caller_id` to `target_id`'s posts (idempotent).
pub async fn subscribe(db: &PgPool, caller_id: i64, target_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO subscriptions (account_id, target_account_id) VALUES ($1, $2) \
         ON CONFLICT (account_id, target_account_id) DO NOTHING",
    )
    .bind(caller_id)
    .bind(target_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Unsubscribe `caller_id` from `target_id`.
pub async fn unsubscribe(db: &PgPool, caller_id: i64, target_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM subscriptions WHERE account_id = $1 AND target_account_id = $2")
        .bind(caller_id)
        .bind(target_id)
        .execute(db)
        .await?;
    Ok(())
}
