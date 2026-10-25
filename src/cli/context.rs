//! Resource tiers and actor resolution for one-shot CLI commands.
//!
//! Commands acquire only what they need: `config list`/`openapi`
//! touch nothing, config/instance/migration/list commands need only
//! Postgres, mutations and job dispatch add Redis and the shared
//! `AppState`, and the ActivityPub context is built only when a
//! command's work needs it (auto-follow, purges, the blocked-domain
//! snapshot, AP-coupled job handlers).

use std::path::Path;

use serde_json::Value;
use sqlx::PgPool;

use crate::{
    activitypub::{self, ApContext},
    cache::{self, Redis},
    config::Config,
    db,
    http::AppState,
    Error, Result,
};

/// Postgres-only context (config reads, migrations, listings).
pub struct DbContext {
    pub config: Config,
    pub pool: PgPool,
}

/// Full runtime: shared `AppState` plus the AP context when built.
pub struct RuntimeContext {
    pub config: Config,
    pub pool: PgPool,
    #[allow(dead_code)]
    pub redis: Redis,
    pub state: AppState,
    pub ap: Option<ApContext>,
}

/// Effective-config commands: `Config::load_full` already opens the
/// pool it needs to read `config_overlays` — reuse it rather than
/// opening a second one.
pub async fn load_db(path: Option<&Path>) -> Result<DbContext> {
    let (config, pool) = Config::load_full(path).await?;
    Ok(DbContext { config, pool })
}

/// Overlay-editing context for `config set`/`unset`: resolves the
/// base config (TOML + env) and reads the overlay rows *without*
/// validating the current effective config, so a broken overlay can
/// be corrected or removed. Callers validate the candidate before
/// writing; migrations are never auto-run from a config command.
pub async fn load_overlay_editor(
    path: Option<&Path>,
) -> Result<(Config, PgPool, Vec<(String, Value)>)> {
    let preliminary = Config::load(path)?;
    let pool = db::connect(&preliminary.database).await?;
    let overlays = db::config_overlays::fetch(&pool).await?;
    Ok((preliminary, pool, overlays))
}

/// Full runtime context: effective config, Redis, `AppState`, and —
/// only when `with_ap` and `federation.enabled` — the ActivityPub
/// context (which provisions the instance actor).
pub async fn load_runtime(path: Option<&Path>, with_ap: bool) -> Result<RuntimeContext> {
    let DbContext { config, pool } = load_db(path).await?;
    let redis = cache::connect(&config.redis).await?;
    let mut state = AppState::new(config.clone(), pool.clone(), redis.clone());
    let mut ap = None;
    if with_ap && config.federation.enabled {
        let ctx = activitypub::build(&config, &pool, &redis, &state.dispatcher, &state.bus).await?;
        state.ap = Some(ctx.clone());
        ap = Some(ctx);
    }
    Ok(RuntimeContext {
        config,
        pool,
        redis,
        state,
        ap,
    })
}

/// A resolved audit actor: the account id and its real `accounts.role`.
#[derive(Debug)]
pub struct Actor {
    pub account_id: i64,
    pub role: String,
}

/// The instance actor row, when provisioned (`yonder serve` with
/// federation enabled creates it; `user create` alone does not).
async fn instance_actor(pool: &PgPool) -> Result<Option<Actor>> {
    Ok(sqlx::query_as::<_, (i64, String)>(
        "SELECT id, role FROM accounts \
         WHERE username = 'yonder.instance' AND domain IS NULL AND deleted_at IS NULL",
    )
    .fetch_optional(pool)
    .await?
    .map(|(account_id, role)| Actor { account_id, role }))
}

/// Resolve the actor a command's mutations are attributed to.
/// `--actor <username>` names a local, non-deleted, non-suspended
/// **admin** account; without it the default is the instance actor.
/// Never synthesizes an id — a missing instance actor fails with
/// guidance rather than guessing.
pub async fn resolve_actor(pool: &PgPool, username: Option<&str>) -> Result<Actor> {
    match username {
        Some(name) => {
            let row = sqlx::query_as::<_, (i64, String)>(
                "SELECT id, role FROM accounts \
                 WHERE lower(username) = lower($1) AND domain IS NULL \
                   AND deleted_at IS NULL AND suspended_at IS NULL",
            )
            .bind(name)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| Error::NotFound(format!("local account {name:?}")))?;
            if row.1 != "admin" {
                return Err(Error::Validation(format!(
                    "--actor {name:?} is not an admin account"
                )));
            }
            Ok(Actor {
                account_id: row.0,
                role: row.1,
            })
        }
        None => instance_actor(pool).await?.ok_or_else(|| {
            Error::Validation(
                "no instance actor is provisioned — pass --actor, or start the \
                 server once with federation enabled"
                    .to_string(),
            )
        }),
    }
}

/// The audit-actor id for operations allowed to run without one
/// (bootstrap `user create`, overlay writes): the instance actor
/// when provisioned, the explicit `--actor` admin when supplied,
/// `None` otherwise. Commands that require a non-null actor use
/// [`resolve_actor`] instead.
pub async fn resolve_audit_actor(pool: &PgPool, username: Option<&str>) -> Result<Option<i64>> {
    match username {
        Some(_) => Ok(Some(resolve_actor(pool, username).await?.account_id)),
        None => Ok(instance_actor(pool).await?.map(|a| a.account_id)),
    }
}
