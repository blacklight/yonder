//! `yonder frontend` — install, remove and list alternative
//! frontends through the shared `frontends::install` service (zip,
//! filesystem path or git clone).

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{json, Value};

use crate::{frontends, Error, Result};

use super::{confirm, context, output::Output};

#[derive(Debug, Subcommand)]
pub enum FrontendCommand {
    /// Install a frontend from a zip file, an https/ssh git URL, or a
    /// directory inside `frontends.install_from_path_root`.
    Install {
        /// `.zip` file, git URL, or local directory.
        source: String,
        /// Registry name (derived from the source basename when
        /// omitted).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Version label recorded in the manifest.
        #[arg(long)]
        version: Option<String>,
        /// Git branch/tag to clone (git sources only).
        #[arg(long, value_name = "REF")]
        r#ref: Option<String>,
    },
    /// Remove an installed frontend.
    Remove {
        /// Frontend name.
        name: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List installed frontends.
    List,
}

pub async fn run(path: Option<&Path>, command: FrontendCommand, out: &Output) -> Result<u8> {
    match command {
        FrontendCommand::Install {
            source,
            name,
            version,
            r#ref,
        } => {
            install(
                path,
                &source,
                name.as_deref(),
                version.as_deref(),
                r#ref.as_deref(),
                out,
            )
            .await
        }
        FrontendCommand::Remove { name, yes } => remove(path, &name, yes, out).await,
        FrontendCommand::List => list(path, out).await,
    }
}

/// Derive a registry name from the source's basename (`alt-1.0.zip`
/// → `alt-1.0`, `repo.git` → `repo`) and validate it. The error
/// reports only the basename — a URL-shaped source can carry
/// credentials in earlier segments.
fn derive_name(source: &str) -> Result<String> {
    let base = source
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(source);
    let stem = base
        .strip_suffix(".zip")
        .or_else(|| base.strip_suffix(".git"))
        .unwrap_or(base);
    frontends::validate_name(stem).map_err(|_| {
        Error::Validation(format!(
            "cannot derive a frontend name from {base:?}; pass --name"
        ))
    })?;
    Ok(stem.to_string())
}

/// Whether `source` is a git-clone URL (the only source kinds allowed
/// to carry a scheme). Checked before name derivation so a
/// credential-bearing URL is rejected before it can be echoed.
fn is_git_source(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("ssh://") || source.starts_with("git@")
}

/// Reject https/ssh git URLs carrying credentials, a query or a
/// fragment — they must never reach the clone command line or an
/// error message. An ssh URL may carry a username (`ssh://git@…`),
/// and scp-style `git@host:path` needs no URL parsing.
fn check_git_url(url: &str) -> Result<()> {
    if url.starts_with("git@") {
        return Ok(());
    }
    let parsed =
        url::Url::parse(url).map_err(|_| Error::Validation("invalid git URL".to_string()))?;
    let clean = parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && (parsed.scheme() == "ssh" || parsed.username().is_empty());
    if clean {
        Ok(())
    } else {
        Err(Error::Validation(
            "git URL must not contain credentials, a query or a fragment".to_string(),
        ))
    }
}

async fn install(
    path: Option<&Path>,
    source: &str,
    name: Option<&str>,
    version: Option<&str>,
    r#ref: Option<&str>,
    out: &Output,
) -> Result<u8> {
    let ctx = context::load_db(path).await?;
    if is_git_source(source) {
        check_git_url(source)?;
    }
    let name = match name {
        Some(n) => {
            frontends::validate_name(n)?;
            n.to_string()
        }
        None => derive_name(source)?,
    };
    let config = &ctx.config;
    let src_path = PathBuf::from(source);
    let frontend = if src_path.is_file()
        && src_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        // Bound the read by the same compressed cap the upload route
        // enforces; the uncompressed cap applies inside the service.
        // `take` limits the allocation itself — a file that grows or
        // is swapped after a metadata check cannot slip past it.
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&src_path)?
            .take(config.frontends.max_upload_size + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > config.frontends.max_upload_size {
            return Err(Error::Validation(format!(
                "archive exceeds the upload size cap of {} bytes",
                config.frontends.max_upload_size
            )));
        }
        frontends::install::install_from_zip(config, &name, version, &bytes).await?
    } else if is_git_source(source) {
        frontends::install::install_from_git(config, &name, version, source, r#ref).await?
    } else if src_path.is_dir() {
        frontends::install::install_from_path(config, &name, version, source).await?
    } else {
        return Err(Error::Validation(
            "install source is not a zip file, a git URL, or a directory".to_string(),
        ));
    };
    let entity = frontend_json(&frontend);
    if out.json_mode() {
        out.emit_json(&entity)?;
    } else {
        out.emit_human(&format!(
            "installed {} ({})",
            entity["name"], entity["version"]
        ))?;
    }
    Ok(0)
}

async fn remove(path: Option<&Path>, name: &str, yes: bool, out: &Output) -> Result<u8> {
    let ctx = context::load_db(path).await?;
    // Existence check precedes the prompt: an unknown frontend is
    // not-found (3), not a confirmation refusal (4).
    if frontends::find(&ctx.config, name)?.is_none() {
        return Err(Error::NotFound(format!(
            "frontend {name:?} is not installed"
        )));
    }
    if !confirm::confirm(&format!("remove frontend {name}?"), yes)? {
        return confirm::declined(out);
    }
    frontends::install::remove(&ctx.config, name).await?;
    if out.json_mode() {
        out.emit_json(&json!({"name": name, "removed": true}))?;
    } else {
        out.emit_human(&format!("removed {name}"))?;
    }
    Ok(0)
}

async fn list(path: Option<&Path>, out: &Output) -> Result<u8> {
    let ctx = context::load_db(path).await?;
    let list = frontends::list(&ctx.config)?;
    let entities: Vec<Value> = list.iter().map(frontend_json).collect();
    // The builtin is always resolvable — list it first.
    let builtin = frontend_json(&frontends::builtin(&ctx.config));
    let mut all = vec![builtin];
    all.extend(entities);
    if out.json_mode() {
        out.emit_json(&Value::Array(all))?;
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|f| {
            vec![
                f["name"].as_str().unwrap_or_default().to_string(),
                f["version"].as_str().unwrap_or_default().to_string(),
                f["root"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    out.table(&["name", "version", "root"], &rows)?;
    Ok(0)
}

fn frontend_json(f: &frontends::Frontend) -> Value {
    json!({
        "name": f.name,
        "version": f.manifest.version.clone().unwrap_or_default(),
        "source": f.manifest.source.clone().unwrap_or_default(),
        "root": f.root.to_string_lossy(),
    })
}
