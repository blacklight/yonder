//! `yonder` command line.
//!
//! One binary, subcommand-driven: `serve` (default — HTTP plus the
//! optional in-process worker), `worker` (standalone queue drain +
//! scheduler), `migrate`, `config`, `instance`, `run-job`, `user`,
//! `domain-block`, `relay`, `emoji`, `frontend` and `openapi` —
//! thin adapters over the same service functions the HTTP API uses.
//!
//! Output contract: `--json` prints exactly one JSON value on
//! stdout for result-bearing commands; diagnostics, warnings and
//! errors always go to stderr. Errors map to stable exit codes via
//! [`exit::exit_code`]; clap's own usage errors keep exit code 2.

pub mod config;
pub mod confirm;
pub mod context;
pub mod domain_block;
pub mod emoji;
pub mod exit;
pub mod frontend;
pub mod jobs;
pub mod migrate;
pub mod output;
pub mod relay;
pub mod user;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::{activitypub::ApContext, cache, config::Config, db, http, logging, Error, Result};

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

    /// Emit exactly one JSON value on stdout for result-bearing
    /// commands; errors render as a JSON object on stderr.
    #[arg(long, global = true)]
    pub json: bool,

    /// Attribute mutations (audit-log actor) to this local admin
    /// account instead of the default `yonder.instance` actor.
    #[arg(long, global = true, value_name = "USERNAME")]
    pub actor: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP server (default).
    Serve,
    /// Run the background-job worker (queue drain + scheduler).
    Worker,
    /// Run database migrations, or inspect their status.
    Migrate {
        #[command(subcommand)]
        command: Option<migrate::MigrateCommand>,
        /// Roll back instead of applying: to the given target version
        /// (exclusive), or one step back when omitted.
        #[arg(long, value_name = "VERSION")]
        revert: Option<Option<i64>>,
    },
    /// Inspect and manage instance configuration (database overlays).
    Config {
        #[command(subcommand)]
        command: config::ConfigCommand,
    },
    /// Enqueue a background job, or run one in-process.
    RunJob {
        /// Job kind — one of the names `yonder run-job` reports for an
        /// unknown kind (e.g. `audit_log_cleanup`, `email_send`).
        kind: String,
        /// Job payload as a JSON object (default: {}).
        #[arg(long, value_name = "JSON")]
        payload: Option<String>,
        /// Execute the handler in this process instead of enqueuing.
        /// Bypasses worker retry and dead-letter accounting.
        #[arg(long)]
        inline: bool,
        /// Enqueue the job to fire no earlier than this many seconds
        /// from now (enqueue mode only).
        #[arg(long, value_name = "SECS")]
        delay: Option<u64>,
    },
    /// Administer local user accounts.
    User {
        #[command(subcommand)]
        command: user::UserCommand,
    },
    /// Show or change instance identity settings.
    Instance {
        #[command(subcommand)]
        command: config::InstanceCommand,
    },
    /// Manage instance domain blocks (moderation).
    DomainBlock {
        #[command(subcommand)]
        command: domain_block::DomainBlockCommand,
    },
    /// Manage ActivityPub relays.
    Relay {
        #[command(subcommand)]
        command: relay::RelayCommand,
    },
    /// Import custom emoji.
    Emoji {
        #[command(subcommand)]
        command: emoji::EmojiCommand,
    },
    /// Install and remove alternative frontends.
    Frontend {
        #[command(subcommand)]
        command: frontend::FrontendCommand,
    },
    /// Print the OpenAPI document as JSON on stdout — the input the
    /// frontend's `npm run api:gen` consumes; needs no config or
    /// database.
    Openapi,
}

/// Entry point called by `main`. Reads `.env` if present, dispatches,
/// and owns all error rendering — clap usage errors keep their own
/// stderr format and exit code 2 and never reach this path.
pub async fn main() -> ExitCode {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            output::render_error(&e, json);
            ExitCode::from(exit::exit_code(&e))
        }
    }
}

/// Dispatch one parsed command. `Ok(0)` is success; `Ok(1)` is the
/// `migrate status` "migrations pending" signal (a status, not an
/// error); `Err` renders via `main`.
async fn run(cli: Cli) -> Result<u8> {
    let path = cli.config.as_deref();
    let out = output::Output::new(cli.json);
    let actor = cli.actor.as_deref();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(path).await.map(|_| 0),
        Command::Worker => worker(path).await.map(|_| 0),
        Command::Openapi => openapi().map(|_| 0),
        Command::Migrate { command, revert } => migrate::run(path, command, revert, &out).await,
        Command::Config { command } => config::run(path, command, &out, actor).await,
        Command::Instance { command } => config::run_instance(path, command, &out, actor).await,
        Command::RunJob {
            kind,
            payload,
            inline,
            delay,
        } => jobs::run_job(path, &kind, payload.as_deref(), inline, delay, &out).await,
        Command::User { command } => user::run(path, command, &out, actor).await,
        Command::DomainBlock { command } => domain_block::run(path, command, &out, actor).await,
        Command::Relay { command } => relay::run(path, command, &out, actor).await,
        Command::Emoji { command } => emoji::run(path, command, &out, actor).await,
        Command::Frontend { command } => frontend::run(path, command, &out).await,
    }
}

fn openapi() -> Result<()> {
    let doc = http::openapi::document()
        .to_json()
        .map_err(|e| Error::Internal(format!("openapi serialize failed: {e}")))?;
    println!("{doc}");
    Ok(())
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

/// Every job handler, registered once — the worker (standalone or
/// in-process) and `run-job --inline` share exactly this wiring so an
/// inline run executes the same handler a drained job would. Stubs
/// are registered first; real handlers replace them in domain order.
/// `ap` is `Some` only when federation is enabled — handlers that
/// need the AP context are either AP-gated or degrade to local work.
pub fn build_registry(ap: Option<&ApContext>) -> crate::jobs::Registry {
    let mut registry = crate::jobs::Registry::default();
    crate::jobs::stub::register_all(&mut registry);
    // Epic-09 profile jobs — independent of the federation gate.
    crate::users::profile::register_jobs(&mut registry);
    // Real federation handlers replace the stubs when federation is on.
    if let Some(ap) = ap {
        crate::activitypub::register_handlers(&mut registry, ap);
    }
    // Epic-11 account-management jobs: local work runs regardless of
    // the federation gate; handlers degrade to "no AP emission" when
    // `ap` is None. MigrateFollowers needs AP (actor fetches,
    // Undo/Follow publishes), so it only registers under the gate.
    crate::users::deletion::register_jobs(&mut registry, ap);
    crate::users::transfer::register_jobs(&mut registry, ap);
    if let Some(ap) = ap {
        crate::users::moves::register_jobs(&mut registry, ap);
    }
    // Epic-12 posting jobs (scheduled publish, link previews,
    // scheduled deletion, orphan-media cleanup).
    crate::posts::register_jobs(&mut registry, ap);
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
    registry
}

/// Build the job registry and the worker over it. The returned
/// scheduler must be kept alive for cron jobs to keep firing.
///
/// The worker gets its own Redis pool sized for its blocking
/// consumers: each of the `concurrency` tasks per queue holds a
/// connection for the duration of `XREADGROUP BLOCK`, so the pool
/// needs `queues × concurrency + headroom` connections.
async fn start_worker(
    config: &Config,
    pool: sqlx::PgPool,
    bus: &crate::http::ws::EventBus,
) -> Result<(
    crate::jobs::worker::Worker,
    tokio_cron_scheduler::JobScheduler,
)> {
    let blocking_consumers = config.jobs.concurrency * crate::jobs::Queue::ALL.len() as u32;
    let redis = cache::connect(&crate::config::RedisConfig {
        pool_size: config.redis.pool_size.max(blocking_consumers + 4),
        ..config.redis.clone()
    })
    .await?;
    let dispatcher = crate::jobs::Dispatcher::new(&redis, &config.jobs);
    let mut ap_ctx = None;
    if config.federation.enabled {
        let ap = crate::activitypub::build(config, &pool, &redis, &dispatcher, bus).await?;
        ap_ctx = Some(ap);
    }
    let registry = build_registry(ap_ctx.as_ref());
    let ctx = crate::jobs::JobContext {
        db: pool,
        redis: redis.clone(),
        config: Arc::new(config.clone()),
        media: crate::media::storage::MediaStore::from_config(config),
    };
    let scheduler = crate::jobs::scheduler::start(config, &dispatcher).await?;
    let worker = crate::jobs::worker::Worker::new(
        ctx,
        Arc::new(registry),
        Arc::new(config.jobs.clone()),
        crate::jobs::worker::default_consumer(),
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
