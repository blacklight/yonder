//! `yonder domain-block` — instance moderation CRUD over the shared
//! `moderation::blocks` service, mirroring the HTTP admin handler's
//! transaction and audit shape exactly (`domain_block.create` /
//! `.update` / `.delete`). `private_comment` is moderation-internal
//! and never leaves the process.

use std::path::Path;

use clap::Subcommand;
use serde_json::{json, Value};

use crate::{
    moderation::{
        audit,
        blocks::{self, Severity},
        domain::normalize_domain,
    },
    Error, Result,
};

use super::{confirm, context, output::Output};

#[derive(Debug, Subcommand)]
pub enum DomainBlockCommand {
    /// Create or update a domain block. Defaults to `silence`
    /// (hidden publicly, visible to followers) — pass --severity
    /// suspend to defederate entirely.
    Add {
        /// Domain to block (bare host or URL — normalized).
        domain: String,
        /// Block severity (noop, silence or suspend; default silence).
        #[arg(long)]
        severity: Option<String>,
        /// Do not cache or proxy the domain's media.
        #[arg(long)]
        reject_media: bool,
        /// Drop inbound reports (Flags) from the domain.
        #[arg(long)]
        reject_reports: bool,
        /// Obfuscate the domain in public listings.
        #[arg(long)]
        obfuscate: bool,
        /// Reason shown publicly (≤ 2000 chars).
        #[arg(long)]
        public_comment: Option<String>,
        /// Staff-internal reason (≤ 2000 chars, never printed back).
        #[arg(long)]
        private_comment: Option<String>,
        /// Skip the confirmation prompt (required for --severity
        /// suspend in non-interactive use).
        #[arg(long)]
        yes: bool,
    },
    /// Remove a domain block.
    Remove {
        /// Domain to unblock (bare host or URL — normalized).
        domain: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List domain blocks (public fields only).
    List {
        /// Maximum rows (1–200, default 100).
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
}

/// The public projection every `domain-block` command emits —
/// `private_comment` is deliberately absent.
fn public_fields(row: &blocks::BlockRow) -> Value {
    json!({
        "id": row.id,
        "domain": row.domain,
        "severity": row.severity.as_str(),
        "reject_media": row.reject_media,
        "reject_reports": row.reject_reports,
        "obfuscate": row.obfuscate,
        "public_comment": row.public_comment,
    })
}

pub async fn run(
    path: Option<&Path>,
    command: DomainBlockCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        DomainBlockCommand::Add {
            domain,
            severity,
            reject_media,
            reject_reports,
            obfuscate,
            public_comment,
            private_comment,
            yes,
        } => {
            add(
                path,
                &domain,
                severity.as_deref(),
                reject_media,
                reject_reports,
                obfuscate,
                public_comment.as_deref(),
                private_comment.as_deref(),
                yes,
                out,
                actor,
            )
            .await
        }
        DomainBlockCommand::Remove { domain, yes } => remove(path, &domain, yes, out, actor).await,
        DomainBlockCommand::List { limit } => list(path, limit, out).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    path: Option<&Path>,
    domain: &str,
    severity: Option<&str>,
    reject_media: bool,
    reject_reports: bool,
    obfuscate: bool,
    public_comment: Option<&str>,
    private_comment: Option<&str>,
    yes: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let domain = normalize_domain(domain);
    if domain.is_empty() {
        return Err(Error::Validation("invalid domain".to_string()));
    }
    let severity = match severity {
        None => Severity::Silence,
        Some(s) => Severity::parse(s)
            .ok_or_else(|| Error::Validation(format!("invalid severity {s:?}")))?,
    };
    let private_comment = private_comment.unwrap_or_default();
    let public_comment = public_comment.unwrap_or_default();
    if private_comment.chars().count() > 2000 || public_comment.chars().count() > 2000 {
        return Err(Error::Validation(
            "comments are limited to 2000 characters".into(),
        ));
    }
    if severity == Severity::Suspend
        && !confirm::confirm(&format!("suspend (defederate) {domain}?"), yes)?
    {
        return confirm::declined(out);
    }
    let ctx = context::load_runtime(path, true).await?;
    if domain == normalize_domain(&ctx.config.instance.domain) {
        return Err(Error::Validation(
            "cannot block your own instance".to_string(),
        ));
    }
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let existed = blocks::policy_for(&ctx.pool, &domain).await?.is_some();
    let mut tx = ctx.pool.begin().await?;
    let row = blocks::upsert(
        &mut *tx,
        &domain,
        severity,
        reject_media,
        reject_reports,
        obfuscate,
        private_comment,
        public_comment,
        actor.account_id,
    )
    .await?;
    audit::record_action(
        &mut *tx,
        actor.account_id,
        if existed {
            "domain_block.update"
        } else {
            "domain_block.create"
        },
        "domain",
        row.id,
        &row.private_comment,
        json!({ "domain": row.domain, "severity": row.severity.as_str() }),
    )
    .await?;
    tx.commit().await?;
    // Refresh this process's blocked-domain snapshot; a separately
    // running server still needs the admin API or a restart.
    blocks::refresh_blocked(&ctx.state).await?;

    let entity = public_fields(&row);
    if out.json_mode() {
        out.emit_json(&entity)?;
    } else {
        let verb = if existed { "updated" } else { "added" };
        out.emit_human(&format!(
            "{verb} {} ({})",
            entity["domain"], entity["severity"]
        ))?;
    }
    Ok(0)
}

async fn remove(
    path: Option<&Path>,
    domain: &str,
    yes: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let domain = normalize_domain(domain);
    if domain.is_empty() {
        return Err(Error::Validation("invalid domain".to_string()));
    }
    let ctx = context::load_runtime(path, true).await?;
    let row = blocks::policy_for(&ctx.pool, &domain)
        .await?
        .ok_or_else(|| Error::NotFound(format!("domain block {domain:?}")))?;
    if !confirm::confirm(&format!("remove the block on {domain}?"), yes)? {
        return confirm::declined(out);
    }
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let mut tx = ctx.pool.begin().await?;
    blocks::delete(&mut *tx, row.id).await?;
    audit::record(
        &mut *tx,
        Some(actor.account_id),
        "domain_block.delete",
        "domain",
        Some(row.id),
        json!({ "domain": row.domain }),
    )
    .await?;
    tx.commit().await?;
    blocks::refresh_blocked(&ctx.state).await?;

    if out.json_mode() {
        out.emit_json(&json!({"domain": row.domain, "removed": true}))?;
    } else {
        out.emit_human(&format!("removed {domain}"))?;
    }
    Ok(0)
}

async fn list(path: Option<&Path>, limit: u32, out: &Output) -> Result<u8> {
    if !(1..=200).contains(&limit) {
        return Err(Error::Validation("--limit must be in 1..=200".to_string()));
    }
    let ctx = context::load_db(path).await?;
    let rows = blocks::list(&ctx.pool, None, None, i64::from(limit)).await?;
    if out.json_mode() {
        let entities: Vec<Value> = rows.iter().map(public_fields).collect();
        out.emit_json(&Value::Array(entities))?;
        return Ok(0);
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r.domain.clone(),
                r.severity.as_str().to_string(),
                r.public_comment.clone(),
            ]
        })
        .collect();
    out.table(&["domain", "severity", "public comment"], &table)?;
    Ok(0)
}
