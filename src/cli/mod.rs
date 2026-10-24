//! `yonder` command line.
//!
//! One binary, subcommand-driven: `serve` (default — HTTP plus the
//! optional in-process worker), `worker` (standalone queue drain +
//! scheduler), `migrate`, `config`. Epic 27 grows this with admin
//! subcommands; the config tree already exists so `config list`/
//! `config get` exercise the registry contract.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::{
    cache,
    config::{registry, Config},
    db, http, jobs, logging, Error, Result,
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
    /// Print the value of a configuration key (dotted TOML path) as
    /// resolved from config.toml, env vars and defaults — database
    /// overlays (`config_overlays`) are not consulted.
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
    let redis = cache::connect(&config.redis).await?;
    let mut app_state = http::AppState::new(config.clone(), pool.clone(), redis.clone());
    if config.federation.enabled {
        let ap = crate::activitypub::build(
            &config,
            &pool,
            &redis,
            &app_state.dispatcher,
            &app_state.bus,
        )
        .await?;
        app_state.ap = Some(ap);
    }
    let state = Arc::new(app_state);

    // The stream relay forwards Redis pub/sub events onto the local
    // bus — the job-side producers' only path to WebSocket clients
    // (epic 16). Its task outlives the function via process lifetime;
    // it self-heals on Redis outages.
    let relay_cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(http::ws::relay::run(
        state.redis.clone(),
        state.bus.clone(),
        relay_cancel.clone(),
    ));

    // The in-process worker runs alongside HTTP when enabled. The
    // scheduler must stay bound for the function's lifetime — dropping
    // it stops cron firings.
    let mut worker_parts = if config.jobs.enabled {
        let (worker, scheduler) = start_worker(&config, pool, &state.bus).await?;
        let token = worker.cancel_token();
        let join = tokio::spawn(async move { worker.run().await });
        Some((join, token, scheduler))
    } else {
        None
    };

    // HTTP serves until a termination signal — or until the worker
    // exits early: a dead queue backend shouldn't leave a half-working
    // instance serving 200s.
    let mut worker_join = worker_parts.as_mut().map(|(join, token, _)| (join, token));
    tokio::select! {
        result = http::serve(state, shutdown_signal()) => result?,
        exited = async {
            match &mut worker_join {
                Some((join, _)) => join.await,
                None => std::future::pending().await,
            }
        } => {
            match exited {
                Ok(Ok(())) => tracing::warn!("worker exited early"),
                Ok(Err(e)) => tracing::error!(error = %e, "worker failed"),
                Err(e) => tracing::error!(error = %e, "worker task panicked"),
            }
            return Err(Error::Internal("background worker exited".to_string()));
        }
    }

    if let Some((join, token, _scheduler)) = worker_parts {
        token.cancel();
        join.await.map_err(|e| Error::Internal(e.to_string()))??;
    }
    relay_cancel.cancel();
    Ok(())
}

/// Resolves when the process should shut down: SIGINT (ctrl-c) or, on
/// Unix, SIGTERM — the signal Docker/Kubernetes/systemd send.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler unavailable");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Build the job registry (stubs for now) and the worker over it. The
/// returned scheduler must be kept alive for cron jobs to keep firing.
///
/// The worker gets its own Redis pool sized for its blocking
/// consumers: each of the `concurrency` tasks per queue holds a
/// connection for the duration of `XREADGROUP BLOCK`, so the pool
/// needs `queues × concurrency + headroom` connections.
async fn start_worker(
    config: &Config,
    pool: sqlx::PgPool,
    bus: &crate::http::ws::EventBus,
) -> Result<(jobs::worker::Worker, tokio_cron_scheduler::JobScheduler)> {
    let blocking_consumers = config.jobs.concurrency * jobs::Queue::ALL.len() as u32;
    let redis = cache::connect(&crate::config::RedisConfig {
        pool_size: config.redis.pool_size.max(blocking_consumers + 4),
        ..config.redis.clone()
    })
    .await?;
    let mut registry = jobs::Registry::default();
    jobs::stub::register_all(&mut registry);
    // Epic-09 profile jobs — independent of the federation gate.
    crate::users::profile::register_jobs(&mut registry);
    let dispatcher = jobs::Dispatcher::new(&redis, &config.jobs);
    // Real federation handlers replace the stubs when federation is on.
    let mut ap_ctx = None;
    if config.federation.enabled {
        let ap = crate::activitypub::build(config, &pool, &redis, &dispatcher, bus).await?;
        crate::activitypub::register_handlers(&mut registry, &ap);
        ap_ctx = Some(ap);
    }
    // Epic-11 account-management jobs: local work runs regardless of
    // the federation gate; handlers degrade to "no AP emission" when
    // `ap_ctx` is None. MigrateFollowers needs AP (actor fetches,
    // Undo/Follow publishes), so it only registers under the gate.
    crate::users::deletion::register_jobs(&mut registry, ap_ctx.as_ref());
    crate::users::transfer::register_jobs(&mut registry, ap_ctx.as_ref());
    if let Some(ap) = ap_ctx.as_ref() {
        crate::users::moves::register_jobs(&mut registry, ap);
    }
    // Epic-12 posting jobs (scheduled publish, link previews,
    // scheduled deletion, orphan-media cleanup).
    crate::posts::register_jobs(&mut registry, ap_ctx.as_ref());
    // Epic-14 social jobs (FollowEvent settle — local work regardless
    // of the federation gate).
    crate::social::register_jobs(&mut registry);
    // Epic-15 timeline jobs (fan-out + backfill).
    crate::timelines::register_jobs(&mut registry);
    // Epic-16 notifications (fan-out + email enqueue).
    crate::notifications::register_jobs(&mut registry);

    // Epic 21 — audit log retention sweep.
    crate::moderation::register_jobs(&mut registry);
    // Epic 22 — the real `EmailSend` SMTP sender (replaces the stub).
    crate::admin::register_jobs(&mut registry);
    // Epic-18 media jobs (remote fetch + cleanup sweep).
    crate::media::register_jobs(&mut registry);
    let ctx = jobs::JobContext {
        db: pool,
        redis: redis.clone(),
        config: Arc::new(config.clone()),
        media: crate::media::storage::MediaStore::from_config(config),
    };
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
    // A standalone worker serves no WebSockets — the bus it hands the
    // federation context is detached by design (stream publishes are
    // no-ops; epic 16 can bridge processes later).
    let bus = crate::http::ws::EventBus::default();
    let (worker, _scheduler) = start_worker(&config, pool, &bus).await?;
    tokio::select! {
        result = worker.run() => result?,
        _ = shutdown_signal() => {
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
