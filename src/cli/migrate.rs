//! `yonder migrate` — apply/revert schema migrations and report
//! their status. Migration commands connect with the base config
//! (TOML + env) directly so a missing `config_overlays` table or a
//! broken overlay can never block schema recovery.

use std::path::Path;

use clap::Subcommand;
use serde_json::json;

use crate::{config::Config, db, logging, Error, Result};

use super::output::Output;

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// List every embedded migration as applied or pending. Exits 1
    /// (a status signal, not an error) while migrations are pending.
    Status,
}

pub async fn run(
    path: Option<&Path>,
    command: Option<MigrateCommand>,
    revert: Option<Option<i64>>,
    out: &Output,
) -> Result<u8> {
    if command.is_some() && revert.is_some() {
        return Err(Error::Validation(
            "--revert cannot be combined with a subcommand".to_string(),
        ));
    }
    let config = Config::load(path)?;
    logging::init(&config.server)?;
    let pool = db::connect(&config.database).await?;
    match (command, revert) {
        (Some(MigrateCommand::Status), _) => status(&pool, out).await,
        (None, Some(target)) => {
            db::migrate::revert(&pool, target).await?;
            if out.json_mode() {
                out.emit_json(&json!({"reverted": true, "target": target}))?;
            } else {
                out.emit_human("migrations reverted")?;
            }
            Ok(0)
        }
        (None, None) => {
            db::migrate::run(&pool).await?;
            let versions = db::migrate::applied_versions(&pool).await?;
            tracing::info!(?versions, "migrations applied");
            if out.json_mode() {
                out.emit_json(&json!({"applied": versions}))?;
            } else {
                out.emit_human(&format!("migrations applied: {versions:?}"))?;
            }
            Ok(0)
        }
    }
}

async fn status(pool: &sqlx::PgPool, out: &Output) -> Result<u8> {
    let statuses = db::migrate::status(pool).await?;
    let pending = statuses.iter().filter(|s| !s.applied).count();
    if out.json_mode() {
        let rows: Vec<serde_json::Value> = statuses
            .iter()
            .map(|s| {
                json!({
                    "version": s.version,
                    "description": s.description,
                    "applied": s.applied,
                })
            })
            .collect();
        out.emit_json(&serde_json::Value::Array(rows))?;
    } else {
        let rows: Vec<Vec<String>> = statuses
            .iter()
            .map(|s| {
                vec![
                    s.version.to_string(),
                    s.description.clone(),
                    if s.applied { "applied" } else { "pending" }.to_string(),
                ]
            })
            .collect();
        out.table(&["version", "description", "status"], &rows)?;
        if pending > 0 {
            out.warn(&format!("{pending} migration(s) pending"));
        }
    }
    Ok(if pending > 0 { 1 } else { 0 })
}
