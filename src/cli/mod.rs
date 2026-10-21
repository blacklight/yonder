//! `yonder` command line.
//!
//! One binary, subcommand-driven: `serve` (default), `migrate`, `config`.
//! Epic 27 grows this with worker/admin subcommands; the config tree already
//! exists so `config list`/`config get` exercise the registry contract.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::{
    config::{registry, Config},
    db, logging, Error, Result,
};

#[derive(Debug, Parser)]
#[command(
    name = "yonder",
    version,
    about = "Yonder — a Mastodon-compatible ActivityPub server"
)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, global = true, env = "YONDER_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP server (default).
    Serve,
    /// Run database migrations.
    Migrate,
    /// Inspect instance configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// List every configuration key as `toml.path` ↔ `YONDER_ENV_NAME`.
    List,
    /// Print the effective value of a configuration key (dotted TOML path).
    Get { key: String },
}

/// Entry point called by `main`. Reads `.env` if present, then dispatches.
pub async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let path = cli.config.as_deref();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(path).await,
        Command::Migrate => migrate(path).await,
        Command::Config { command } => config(path, command),
    }
}

async fn serve(path: Option<&Path>) -> Result<()> {
    let (config, _pool) = Config::load_full(path).await?;
    logging::init(&config.server)?;
    tracing::info!(
        domain = %config.instance.domain,
        "resolved effective configuration"
    );
    tracing::info!("HTTP server lands in epic 04 — nothing to serve yet");
    Ok(())
}

async fn migrate(path: Option<&Path>) -> Result<()> {
    let config = Config::load(path)?;
    logging::init(&config.server)?;
    let _pool = db::connect(&config.database).await?;
    tracing::info!("no migrations defined yet — schema lands in epic 02");
    Ok(())
}

fn config(path: Option<&Path>, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::List => {
            for entry in registry::entries() {
                println!("{:<58} {}", entry.toml_path, entry.env);
            }
            Ok(())
        }
        ConfigCommand::Get { key } => {
            if registry::find(&key).is_none() {
                return Err(Error::NotFound(format!("config key {key:?}")));
            }
            let config = Config::load(path)?;
            let value =
                serde_json::to_value(&config).map_err(|e| Error::Internal(e.to_string()))?;
            let leaf = key
                .split('.')
                .try_fold(&value, |node, part| node.get(part))
                .expect("key exists in the registry");
            println!(
                "{}",
                serde_json::to_string_pretty(leaf).map_err(|e| Error::Internal(e.to_string()))?
            );
            Ok(())
        }
    }
}
