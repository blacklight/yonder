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
