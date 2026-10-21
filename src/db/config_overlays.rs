//! `config_overlays` read path for the configuration loader.
//!
//! The table (`key` dotted TOML path → `value` JSONB) is created in epic 02;
//! until then the guarded lookup below makes this a no-op returning an
//! empty list rather than an error.

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
