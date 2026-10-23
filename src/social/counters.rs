//! Counter repair — `recount` recomputes a *local* account's
//! `followers_count`/`following_count` from `follows`. Routine
//! maintenance is the migration-`0016` triggers' job; this exists for
//! repair after bulk imports, moderation actions and drift debugging.
//! Remote accounts are skipped: their counts come from actor docs.

use sqlx::PgPool;

use crate::Result;

/// Recompute `account_id`'s counters from `follows`. No-op for remote
/// or missing accounts; returns whether a row was updated.
pub async fn recount(db: &PgPool, account_id: i64) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE accounts a SET \
            following_count = (SELECT count(*) FROM follows f WHERE f.account_id = a.id), \
            followers_count = (SELECT count(*) FROM follows f WHERE f.target_account_id = a.id) \
         WHERE a.id = $1 AND a.domain IS NULL",
    )
    .bind(account_id)
    .execute(db)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Recompute every local account — the one-shot repair form.
pub async fn recount_all(db: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE accounts a SET \
            following_count = (SELECT count(*) FROM follows f WHERE f.account_id = a.id), \
            followers_count = (SELECT count(*) FROM follows f WHERE f.target_account_id = a.id) \
         WHERE a.domain IS NULL",
    )
    .execute(db)
    .await?;
    Ok(done.rows_affected())
}
