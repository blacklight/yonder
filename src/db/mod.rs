//! PostgreSQL access via `sqlx`.
//!
//! This module ships the pool helper, the embedded schema migrations, and
//! the guarded `config_overlays` read the config loader needs. All queries
//! are runtime-checked (`sqlx::query*`, never `query!` macros) so builds
//! don't require a live `DATABASE_URL`.

pub mod config_overlays;
pub mod migrate;

use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool};

use crate::{config::DatabaseConfig, Result};

/// Connect to Postgres with the configured pool bounds.
pub async fn connect(config: &DatabaseConfig) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_seconds))
        .idle_timeout(Duration::from_secs(config.idle_timeout_seconds))
        .connect(&config.url)
        .await
        .map_err(Into::into)
}
