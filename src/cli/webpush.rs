//! `yonder webpush` — VAPID keypair management for Web Push.
//!
//! `generate-keys` mints a fresh RFC 8292 P-256 pair. Bare, it only
//! prints the keys and needs no config, database or running server —
//! usable before first boot; the private half goes to stderr like
//! every CLI credential. `--store` instead writes the
//! `http.vapid_public_key`/`http.vapid_private_key` overlays and
//! audits the rotation through the same `admin::webpush` service the
//! HTTP API calls, so the key never leaves the process.

use std::path::Path;

use clap::Subcommand;
use serde_json::json;

use crate::{admin::webpush, Result};

use super::{context, output::Output};

#[derive(Debug, Subcommand)]
pub enum WebpushCommand {
    /// Mint a fresh VAPID keypair. Prints the public key on stdout and
    /// the private key on stderr (or both in the `--json` value); with
    /// `--store`, persists the pair as `http.vapid_*` overlays and
    /// audits the rotation instead of printing the private key.
    GenerateKeys {
        /// Store the pair as `http.vapid_public_key` /
        /// `http.vapid_private_key` config overlays — the same path as
        /// the admin "generate keys" action. Needs the database and
        /// Redis; the private key is then never printed.
        #[arg(long)]
        store: bool,
    },
}

pub async fn run(
    path: Option<&Path>,
    command: WebpushCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        WebpushCommand::GenerateKeys { store } => generate_keys(path, store, out, actor).await,
    }
}

async fn generate_keys(
    path: Option<&Path>,
    store: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    if store {
        let ctx = context::load_runtime(path, false).await?;
        let actor_id = context::resolve_audit_actor(&ctx.pool, actor).await?;
        let stored = webpush::generate_and_store(&ctx.state, actor_id).await?;
        if out.json_mode() {
            out.emit_json(&stored)?;
        } else {
            out.emit_human(&format!(
                "VAPID keypair stored as http.vapid_public_key / http.vapid_private_key\n\
                 public key: {}",
                stored["public_key"].as_str().unwrap_or_default()
            ))?;
        }
        return Ok(0);
    }

    let (public, private) = webpush::generate();
    if out.json_mode() {
        out.emit_json(&json!({
            "public_key": public,
            "private_key": private,
        }))?;
    } else {
        out.emit_human(&format!("public key:  {public}"))?;
        // The private half is a credential: stderr only, never stdout.
        out.warn(&format!("private key: {private}"));
        out.warn(
            "install via http.vapid_public_key / http.vapid_private_key in config.toml, \
             the YONDER_HTTP__VAPID_* env vars, `yonder config set`, or rerun with --store",
        );
    }
    Ok(0)
}
