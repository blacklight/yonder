//! Embedded schema migrations (`migrations/`), applied via `sqlx::migrate!`.
//!
//! `run` is idempotent — already-applied versions are skipped — so both
//! `yonder migrate` and serve-time auto-migration are safe to call on every
//! boot. `revert` walks the reversible `.down.sql` files back to a target
//! version.

use sqlx::migrate::Migrator;
use sqlx::PgPool;

use crate::Result;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Apply every pending migration.
pub async fn run(pool: &PgPool) -> Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Roll back applied migrations until `target` (every migration with a
/// higher version is reverted). `None` reverts a single step.
pub async fn revert(pool: &PgPool, target: Option<i64>) -> Result<()> {
    let target = match target {
        Some(v) => v,
        None => {
            let applied: Vec<i64> =
                sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(pool)
                    .await?;
            match applied.len() {
                0 => return Ok(()),
                // Reverting the only applied migration goes back to 0.
                1 => 0,
                _ => applied[applied.len() - 2],
            }
        }
    };
    MIGRATOR.undo(pool, target).await?;
    Ok(())
}

/// Versions of all applied migrations, ascending.
pub async fn applied_versions(pool: &PgPool) -> Result<Vec<i64>> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}
