//! `yonder` command line.
//!
//! One binary, subcommand-driven: `serve` (default), `migrate`, `config`.
//! Epic 27 grows this with worker/admin subcommands; the config tree already
//! exists so `config list`/`config get` exercise the registry contract.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::{
    cache,
    config::{registry, Config},
    db, jobs, logging, Error, Result,
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
    /// Run the background-job worker (queue drain + scheduler).
    Worker,
    /// Run database migrations.
    Migrate {
        /// Roll back instead of applying: to the given target version
        /// (exclusive), or one step back when omitted.
        #[arg(long, value_name = "VERSION")]
        revert: Option<Option<i64>>,
    },
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
        Command::Worker => worker(path).await,
        Command::Migrate { revert } => migrate(path, revert).await,
        Command::Config { command } => config(path, command),
    }
}

async fn serve(path: Option<&Path>) -> Result<()> {
    let (config, pool) = Config::load_full(path).await?;
    logging::init(&config.server)?;
    tracing::info!(
        domain = %config.instance.domain,
        "resolved effective configuration"
    );
    if config.server.migrate_on_start {
        db::migrate::run(&pool).await?;
        tracing::info!("schema migrations applied");
    }
    if config.jobs.enabled {
        let (worker, _scheduler) = start_worker(&config, pool).await?;
        tokio::select! {
            result = worker.run() => result?,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received");
                worker.cancel_token().cancel();
            }
        }
        return Ok(());
    }
    tracing::info!("HTTP server lands in epic 04 — nothing to serve yet");
    Ok(())
}

/// Build the job registry (stubs for now) and the worker over it. The
/// returned scheduler must be kept alive for cron jobs to keep firing.
async fn start_worker(
    config: &Config,
    pool: sqlx::PgPool,
) -> Result<(jobs::worker::Worker, tokio_cron_scheduler::JobScheduler)> {
    let redis = cache::connect(&config.redis).await?;
    let mut registry = jobs::Registry::default();
    jobs::stub::register_all(&mut registry);
    let ctx = jobs::JobContext {
        db: pool,
        redis: redis.clone(),
        config: Arc::new(config.clone()),
    };
    let dispatcher = jobs::Dispatcher::new(&redis, &config.jobs);
    let scheduler = jobs::scheduler::start(config, &dispatcher).await?;
    let worker = jobs::worker::Worker::new(
        ctx,
        Arc::new(registry),
        Arc::new(config.jobs.clone()),
        jobs::worker::default_consumer(),
    );
    Ok((worker, scheduler))
}

async fn worker(path: Option<&Path>) -> Result<()> {
    let (config, pool) = Config::load_full(path).await?;
    logging::init(&config.server)?;
    let (worker, _scheduler) = start_worker(&config, pool).await?;
    tokio::select! {
        result = worker.run() => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
            worker.cancel_token().cancel();
        }
    }
    Ok(())
}

async fn migrate(path: Option<&Path>, revert: Option<Option<i64>>) -> Result<()> {
    let config = Config::load(path)?;
    logging::init(&config.server)?;
    let pool = db::connect(&config.database).await?;
    match revert {
        Some(target) => {
            db::migrate::revert(&pool, target).await?;
            tracing::info!(target = ?target, "migrations reverted");
        }
        None => {
            db::migrate::run(&pool).await?;
            let versions = db::migrate::applied_versions(&pool).await?;
            tracing::info!(?versions, "migrations applied");
        }
    }
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
