//! `yonder emoji import` — bulk custom-emoji imports from a zip
//! archive, a directory under `admin.emoji_import_dir`, or a single
//! image file, all through the shared `emoji::import` service and
//! its zip-bomb/path confinement guards.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{json, Value};

use crate::{emoji::import, media::storage::MediaStore, Error, Result};

use super::{context, output::Output};

#[derive(Debug, Subcommand)]
pub enum EmojiCommand {
    /// Import emoji from a zip archive, a directory inside
    /// `admin.emoji_import_dir`, or a single image file.
    Import {
        /// Path to a `.zip`, a directory, or an image file.
        path: PathBuf,
        /// Assign imported emoji to this pack id.
        #[arg(long)]
        pack_id: Option<i64>,
        /// Replace existing local shortcodes instead of skipping them.
        #[arg(long)]
        overwrite: bool,
    },
}

pub async fn run(
    path: Option<&Path>,
    command: EmojiCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        EmojiCommand::Import {
            path: import_path,
            pack_id,
            overwrite,
        } => import_cmd(path, &import_path, pack_id, overwrite, out, actor).await,
    }
}

async fn import_cmd(
    path: Option<&Path>,
    import_path: &Path,
    pack_id: Option<i64>,
    overwrite: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_db(path).await?;
    let uploader = context::resolve_actor(&ctx.pool, actor)
        .await
        .map_err(|_| {
            Error::Validation(
                "no uploader account available — pass --actor, or start the \
                 server once with federation enabled"
                    .to_string(),
            )
        })?;
    let store = MediaStore::from_config(&ctx.config);

    let outcomes: Vec<Value> = if import_path.is_dir() {
        // `from_dir` always resolves inside `admin.emoji_import_dir`;
        // an absolute path already under that root is rewritten to
        // its relative form, anything else passes through to the
        // service's own confinement check.
        let dir = ctx
            .config
            .admin
            .emoji_import_dir
            .as_ref()
            .and_then(|root| import_path.strip_prefix(Path::new(root)).ok())
            .map_or_else(
                || import_path.to_string_lossy().to_string(),
                |rel| rel.to_string_lossy().to_string(),
            );
        import::from_dir(
            &ctx.pool,
            &store,
            &ctx.config,
            &dir,
            uploader.account_id,
            pack_id,
            overwrite,
        )
        .await?
        .iter()
        .map(outcome_json)
        .collect()
    } else if import_path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    {
        // The compressed file is capped by the same upload bound the
        // HTTP path enforces; the service's own per-entry and
        // uncompressed caps still apply inside. `take` bounds the
        // allocation itself — a file that grows or is swapped after a
        // metadata check cannot slip past it.
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(import_path)?
            .take(ctx.config.admin.max_import_size + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > ctx.config.admin.max_import_size {
            return Err(Error::Validation(format!(
                "archive exceeds the import size cap of {} bytes",
                ctx.config.admin.max_import_size
            )));
        }
        import::import_zip(
            &ctx.pool,
            &store,
            &ctx.config,
            bytes,
            uploader.account_id,
            pack_id,
            overwrite,
        )
        .await?
        .iter()
        .map(outcome_json)
        .collect()
    } else if import_path.is_file() {
        let row = import::from_file(
            &ctx.pool,
            &store,
            &ctx.config,
            import_path,
            uploader.account_id,
            pack_id,
            overwrite,
        )
        .await?;
        if out.json_mode() {
            out.emit_json(&emoji_json(&row))?;
        } else {
            out.emit_human(&format!("imported :{}:", row.shortcode))?;
        }
        return Ok(0);
    } else {
        return Err(Error::NotFound(format!("path {}", import_path.display())));
    };

    let imported = outcomes
        .iter()
        .filter(|o| o["ok"] == Value::Bool(true))
        .count();
    if out.json_mode() {
        out.emit_json(&Value::Array(outcomes))?;
    } else {
        for o in &outcomes {
            if o["ok"] == Value::Bool(true) {
                out.emit_human(&format!("ok: {} (:{}:)", o["name"], o["shortcode"]))?;
            } else {
                out.emit_human(&format!("skipped: {} ({})", o["name"], o["error"]))?;
            }
        }
        out.emit_human(&format!("imported {imported} of {}", outcomes.len()))?;
    }
    Ok(0)
}

fn outcome_json(o: &import::ImportOutcome) -> Value {
    match &o.result {
        Ok(row) => json!({"name": o.name, "ok": true, "shortcode": row.shortcode}),
        Err(msg) => json!({"name": o.name, "ok": false, "error": msg}),
    }
}

fn emoji_json(row: &crate::emoji::model::EmojiRow) -> Value {
    json!({
        "id": row.id,
        "shortcode": row.shortcode,
        "pack_id": row.pack_id,
        "ok": true,
    })
}
