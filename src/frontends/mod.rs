//! Alternative-frontend registry.
//!
//! Custom frontends (Pleroma/Akkoma-style) live as directories under
//! `frontends.dir`, one subdirectory per frontend containing a
//! `frontend.json` manifest. The built-in `yonder` frontend is served
//! from `frontends.builtin_dir` and always resolves, so the server has
//! a servable entry point before any alternative frontend is
//! installed or before `frontend/dist` has been built.
//!
//! Kept as a leaf: depends on `config` and `error` only so `users`,
//! `http` and the admin API can use it without cycles.

pub mod install;

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::Config;
use crate::{Error, Result};

/// Name of the built-in frontend — the value `admin.default_frontend`
/// ships with.
pub const BUILTIN: &str = "yonder";

/// `frontend.json` inside an installed frontend directory.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Frontend name — must match the directory name and
    /// [`validate_name`].
    pub name: String,
    /// Free-form version label shown in the admin UI.
    #[serde(default)]
    pub version: Option<String>,
    /// How it was installed: `builtin`, `zip`, `path` or `git`.
    #[serde(default)]
    pub source: Option<String>,
    /// Entry document relative to the frontend root. Install
    /// normalization guarantees it sits at the root.
    #[serde(default = "default_entry")]
    pub entry: String,
}

fn default_entry() -> String {
    "index.html".to_string()
}

/// A resolved frontend: registry name + filesystem root + manifest.
#[derive(Debug, Clone)]
pub struct Frontend {
    pub name: String,
    pub root: PathBuf,
    pub manifest: Manifest,
}

/// Valid frontend names: `^[a-z0-9_-]+$`, max 64 chars. Keeps names
/// safe for directory use and for the `users.frontend` column.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(
            "frontend name must match [a-z0-9_-]+ and be at most 64 chars".into(),
        ))
    }
}

/// Read and validate the `frontend.json` inside `dir`.
fn read_manifest(dir: &Path, dirname: &str) -> Result<Manifest> {
    let path = dir.join("frontend.json");
    let raw = std::fs::read_to_string(&path)?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .map_err(|e| Error::Validation(format!("invalid frontend.json in {dir:?}: {e}")))?;
    if manifest.name != dirname {
        return Err(Error::Validation(format!(
            "frontend manifest name {:?} does not match directory {dirname:?}",
            manifest.name
        )));
    }
    validate_name(&manifest.name)?;
    Ok(manifest)
}

/// Installed frontends under `frontends.dir` — one directory per
/// frontend, identified by a parseable `frontend.json` whose `name`
/// matches the directory. Malformed entries are skipped with a
/// warning, never fatal; a missing root yields an empty list.
pub fn list(config: &Config) -> Result<Vec<Frontend>> {
    let root = Path::new(&config.frontends.dir);
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        let dirname = entry.file_name().to_string_lossy().to_string();
        match read_manifest(&path, &dirname) {
            Ok(manifest) => out.push(Frontend {
                name: dirname,
                root: path,
                manifest,
            }),
            Err(e) => {
                tracing::warn!(dir = %dirname, error = %e, "skipping invalid frontend")
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The built-in `yonder` frontend — served from
/// `frontends.builtin_dir` (`frontend/dist` after `npm run build`). A
/// missing `index.html` is not an error here: the server answers with
/// a placeholder page instead.
pub fn builtin(config: &Config) -> Frontend {
    Frontend {
        name: BUILTIN.to_string(),
        root: PathBuf::from(&config.frontends.builtin_dir),
        manifest: Manifest {
            name: BUILTIN.to_string(),
            version: None,
            source: Some("builtin".to_string()),
            entry: default_entry(),
        },
    }
}

/// Resolve a single frontend by name — the builtin, or an installed
/// one. `None` for unknown names.
///
/// Reads only `frontends.dir/<name>/frontend.json` rather than
/// scanning the whole registry: the SPA fallback calls this on every
/// request, so resolution stays O(1) in the number of installed
/// frontends. Malformed directories are skipped like `list()` does.
pub fn find(config: &Config, name: &str) -> Result<Option<Frontend>> {
    if name == BUILTIN {
        return Ok(Some(builtin(config)));
    }
    if validate_name(name).is_err() {
        return Ok(None);
    }
    let path = Path::new(&config.frontends.dir).join(name);
    if !path.is_dir() || path.is_symlink() {
        return Ok(None);
    }
    match read_manifest(&path, name) {
        Ok(manifest) => Ok(Some(Frontend {
            name: name.to_string(),
            root: path,
            manifest,
        })),
        Err(e) => {
            tracing::warn!(dir = %name, error = %e, "skipping invalid frontend");
            Ok(None)
        }
    }
}

/// Which frontend a request should be served.
///
/// Order: an installed `user_choice` → an installed
/// `admin.default_frontend` → the builtin. Never fails — resolution
/// always lands on something servable, and per-user references to a
/// since-removed frontend silently fall back.
pub fn resolve_active(config: &Config, user_choice: Option<&str>) -> Frontend {
    if let Some(choice) = user_choice {
        if let Ok(Some(f)) = find(config, choice) {
            return f;
        }
    }
    if let Ok(Some(f)) = find(config, &config.admin.default_frontend) {
        return f;
    }
    builtin(config)
}

/// Helpers shared by the registry and install unit tests.
#[cfg(test)]
pub(crate) mod testutil {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use crate::config::Config;

    pub fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yonder-frontends-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn config_in(dir: &Path) -> Config {
        let mut config = Config::default();
        config.frontends.dir = dir.join("frontends").to_string_lossy().into();
        config.frontends.builtin_dir = dir.join("builtin").to_string_lossy().into();
        config
    }

    pub fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in files {
            writer
                .start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{config_in, temp_dir};
    use super::*;

    fn install(dir: &Path, name: &str, manifest: &str) {
        let path = dir.join("frontends").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("frontend.json"), manifest).unwrap();
        std::fs::write(path.join("index.html"), "<html></html>").unwrap();
    }

    #[test]
    fn validate_name_rules() {
        for ok in ["yonder", "my-frontend", "alt_v2", "x", &"a".repeat(64)] {
            validate_name(ok).unwrap();
        }
        for bad in ["", "Name", "a/b", "a.b", "a b", "..", &"a".repeat(65)] {
            validate_name(bad).unwrap_err();
        }
    }

    #[test]
    fn list_discovers_and_skips_malformed() {
        let dir = temp_dir();
        let config = config_in(&dir);
        install(&dir, "beta", r#"{"name": "beta"}"#);
        install(&dir, "alpha", r#"{"name": "alpha", "version": "1.0"}"#);
        // Malformed: missing manifest, name mismatch, bad JSON.
        std::fs::create_dir_all(dir.join("frontends/no-manifest")).unwrap();
        install(&dir, "mismatch", r#"{"name": "other"}"#);
        install(&dir, "bad-json", "not json");

        let found = list(&config).unwrap();
        assert_eq!(
            found.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(found[0].manifest.version.as_deref(), Some("1.0"));
        assert_eq!(found[0].manifest.entry, "index.html");
    }

    #[test]
    fn find_resolves_builtin_and_installed() {
        let dir = temp_dir();
        let config = config_in(&dir);
        install(&dir, "alt", r#"{"name": "alt"}"#);

        assert!(find(&config, "yonder").unwrap().is_some());
        assert!(find(&config, "alt").unwrap().is_some());
        assert!(find(&config, "missing").unwrap().is_none());
    }

    #[test]
    fn resolve_active_prefers_user_then_default_then_builtin() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        install(&dir, "alt", r#"{"name": "alt"}"#);
        install(&dir, "pick", r#"{"name": "pick"}"#);
        config.admin.default_frontend = "alt".to_string();

        assert_eq!(resolve_active(&config, Some("pick")).name, "pick");
        assert_eq!(resolve_active(&config, Some("gone")).name, "alt");
        assert_eq!(resolve_active(&config, None).name, "alt");

        config.admin.default_frontend = "gone".to_string();
        assert_eq!(resolve_active(&config, None).name, "yonder");
    }
}
