//! `yonder relay` — ActivityPub relay administration over the shared
//! `admin::relays` service (mutation + audit commit atomically).
//! Relay URLs must be clean absolute HTTPS: credentials and
//! fragments are rejected before the service sees them, so they can
//! never be echoed into errors, output or audit rows.

use std::path::Path;

use clap::Subcommand;
use serde_json::{json, Value};

use crate::{admin::relays, Error, Result};

use super::{confirm, context, output::Output};

#[derive(Debug, Subcommand)]
pub enum RelayCommand {
    /// Add a relay by inbox URL — stored `pending` until authorized.
    Add {
        /// Absolute https inbox URL (no credentials or fragment).
        inbox_url: String,
    },
    /// Move a pending relay to `accepted` (manual accept).
    Authorize {
        /// Relay id.
        id: i64,
    },
    /// Remove a relay.
    Remove {
        /// Relay id.
        id: i64,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List relays.
    List,
}

/// Relay URLs must be absolute https with a host, no user-info,
/// query or fragment — the service re-checks, but rejecting here
/// keeps credentials out of service error strings entirely, and the
/// query ban keeps token-bearing URLs out of the stored row, audit
/// details and `relay list` output.
fn validate_relay_url(raw: &str) -> Result<()> {
    let parsed =
        url::Url::parse(raw).map_err(|_| Error::Validation("invalid relay URL".to_string()))?;
    let clean = parsed.scheme() == "https"
        && parsed.host().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if clean {
        Ok(())
    } else {
        Err(Error::Validation(
            "relay URL must be absolute https without credentials, a query or a fragment"
                .to_string(),
        ))
    }
}

/// A stored relay URL rendered without credentials, query or
/// fragment — rows written before validation tightened could still
/// carry secrets, so output never echoes them verbatim.
fn public_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "<invalid url>".to_string(),
    }
}

pub async fn run(
    path: Option<&Path>,
    command: RelayCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        RelayCommand::Add { inbox_url } => {
            validate_relay_url(&inbox_url)?;
            let ctx = context::load_runtime(path, false).await?;
            let actor = context::resolve_actor(&ctx.pool, actor).await?;
            let entity = relays::create(&ctx.state, actor.account_id, &inbox_url).await?;
            emit_one(out, &entity, "added")
        }
        RelayCommand::Authorize { id } => {
            let ctx = context::load_runtime(path, false).await?;
            let actor = context::resolve_actor(&ctx.pool, actor).await?;
            let entity = relays::authorize(&ctx.state, actor.account_id, id).await?;
            emit_one(out, &entity, "authorized")
        }
        RelayCommand::Remove { id, yes } => {
            let ctx = context::load_runtime(path, false).await?;
            // Existence check precedes the prompt: a missing relay is
            // not-found (3), not a confirmation refusal (4).
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM relays WHERE id = $1)")
                    .bind(id)
                    .fetch_one(&ctx.pool)
                    .await?;
            if !exists {
                return Err(Error::NotFound("relay".into()));
            }
            if !confirm::confirm(&format!("remove relay {id}?"), yes)? {
                return confirm::declined(out);
            }
            let actor = context::resolve_actor(&ctx.pool, actor).await?;
            relays::remove(&ctx.state, actor.account_id, id).await?;
            if out.json_mode() {
                out.emit_json(&json!({"id": id, "removed": true}))?;
            } else {
                out.emit_human(&format!("removed relay {id}"))?;
            }
            Ok(0)
        }
        RelayCommand::List => {
            let ctx = context::load_db(path).await?;
            let mut rows = relays::list(&ctx.pool).await?;
            for row in &mut rows {
                if let Some(url) = row.get_mut("inbox_url") {
                    *url = Value::String(public_url(url.as_str().unwrap_or_default()));
                }
            }
            if out.json_mode() {
                out.emit_json(&Value::Array(rows))?;
                return Ok(0);
            }
            let table: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    vec![
                        r["id"].as_str().unwrap_or_default().to_string(),
                        r["state"].as_str().unwrap_or_default().to_string(),
                        r["inbox_url"].as_str().unwrap_or_default().to_string(),
                    ]
                })
                .collect();
            out.table(&["id", "state", "inbox url"], &table)?;
            Ok(0)
        }
    }
}

fn emit_one(out: &Output, entity: &Value, verb: &str) -> Result<u8> {
    let mut entity = entity.clone();
    if let Some(url) = entity.get_mut("inbox_url") {
        *url = Value::String(public_url(url.as_str().unwrap_or_default()));
    }
    if out.json_mode() {
        out.emit_json(&entity)?;
    } else {
        out.emit_human(&format!(
            "{verb} relay {} ({})",
            entity["id"], entity["inbox_url"]
        ))?;
    }
    Ok(0)
}
