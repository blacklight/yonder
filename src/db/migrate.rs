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

/// One embedded migration's state for `yonder migrate status`.
#[derive(Debug)]
pub struct MigrationStatus {
    pub version: i64,
    pub description: String,
    pub applied: bool,
}

/// Per-migration applied/pending view of [`MIGRATOR`]. Only
/// `success = true` rows in `_sqlx_migrations` count as applied — a
/// failed/dirty row stays pending. On a fresh database the
/// migrations table doesn't exist yet and every embedded migration
/// reports pending.
pub async fn status(pool: &PgPool) -> Result<Vec<MigrationStatus>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations')::text")
        .fetch_one(pool)
        .await?;
    let applied: std::collections::HashSet<i64> = if exists.is_some() {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    Ok(MIGRATOR
        .iter()
        // A reversible migration is two entries (Up + Down) sharing a
        // version; only the Up side has an applied/pending meaning.
        .filter(|m| m.migration_type != sqlx::migrate::MigrationType::ReversibleDown)
        .map(|m| MigrationStatus {
            version: m.version,
            description: m.description.to_string(),
            applied: applied.contains(&m.version),
        })
        .collect())
}
