//! `config_overlays` read path for the configuration loader.
//!
//! The table (`key` dotted TOML path → `value` JSONB) is created by the
//! schema migrations; the guarded lookup below degrades to an empty
//! list rather than an error when it doesn't exist yet (fresh installs
//! resolving config before the first `migrate`).

use serde_json::Value;
use sqlx::PgPool;

use crate::Result;

/// Fetch every `(key, value)` overlay pair, or an empty list when the
/// `config_overlays` table doesn't exist yet.
pub async fn fetch(pool: &PgPool) -> Result<Vec<(String, Value)>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('config_overlays')::text")
        .fetch_one(pool)
        .await?;
    if exists.is_none() {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, (String, Value)>("SELECT key, value FROM config_overlays")
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

const UPSERT_SQL: &str = "INSERT INTO config_overlays (key, value) VALUES ($1, $2) \
     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()";

/// Insert or replace one overlay row (epic 22 write path).
pub async fn upsert(pool: &PgPool, key: &str, value: &Value) -> Result<()> {
    sqlx::query(UPSERT_SQL)
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Insert or replace several overlay rows in one transaction, so a
/// related set (the VAPID keypair) never lands half-written.
pub async fn upsert_many(pool: &PgPool, rows: &[(&str, Value)]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (key, value) in rows {
        sqlx::query(UPSERT_SQL)
            .bind(key)
            .bind(value)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Remove one overlay row. Returns `false` when no row existed —
/// reset is idempotent by design.
pub async fn delete(pool: &PgPool, key: &str) -> Result<bool> {
    let deleted = sqlx::query("DELETE FROM config_overlays WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}
