//! Frontend installation — zip upload, server-side filesystem path,
//! or a Git clone — plus removal.
//!
//! Installs always happen in two phases: materialize into a staging
//! directory inside `frontends.dir`, validate and normalize the tree,
//! then atomically rename it into the registry. A failed install never
//! leaves a half-usable frontend behind.
//!
//! Hardening (zip): entries outside the frontend root, symlinks and
//! hardlinks are rejected; a headers-only pass enforces the total
//! uncompressed cap *before* extraction and reads stay bounded per
//! entry — the same model `src/emoji/import.rs` uses for emoji packs.
//! Hardening (git): disabled unless `frontends.git_enabled`, only
//! `https:`/`ssh:` URLs, hooks disabled, no interactive prompts, and
//! the remote stderr never reaches the API response.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::{Error, Result};

use super::{validate_name, Frontend, BUILTIN};

/// Wall-clock cap for `git clone` — the install request must not hang
/// on a remote that accepts TCP but never answers.
const GIT_CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// `frontend.json` written for an install — `source` records where it
/// came from for the admin UI.
fn write_manifest(root: &Path, name: &str, version: Option<&str>, source: &str) -> Result<()> {
    let manifest = serde_json::json!({
        "name": name,
        "version": version,
        "source": source,
        "entry": "index.html",
    });
    let body = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| Error::Internal(format!("manifest serialize failed: {e}")))?;
    std::fs::write(root.join("frontend.json"), body)?;
    Ok(())
}

/// Fresh staging directory inside the registry root (same filesystem,
/// so the final `rename` is atomic).
fn staging_dir(config: &Config, name: &str) -> Result<PathBuf> {
    let dir = Path::new(&config.frontends.dir);
    std::fs::create_dir_all(dir)?;
    Ok(dir.join(format!(".staging-{name}-{}", uuid::Uuid::new_v4())))
}

fn cleanup(dir: &Path) {
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Name already taken? The builtin and any installed frontend
/// conflict — installs never overwrite.
fn ensure_free(config: &Config, name: &str) -> Result<()> {
    if name == BUILTIN || super::find(config, name)?.is_some() {
        return Err(Error::Conflict(format!(
            "a frontend named {name:?} is already installed"
        )));
    }
    Ok(())
}

/// Promote a validated staging directory into the registry.
fn promote(config: &Config, name: &str, staging: &Path) -> Result<Frontend> {
    let target = Path::new(&config.frontends.dir).join(name);
    std::fs::rename(staging, &target)?;
    let frontend = super::find(config, name)?
        .ok_or_else(|| Error::Internal(format!("installed frontend {name:?} not discoverable")))?;
    Ok(frontend)
}

/// If `root` has no `index.html` but exactly one child directory
/// does, hoist that directory's contents to the root — build outputs
/// and GitHub archives commonly wrap everything in one folder.
fn normalize(root: &Path) -> Result<()> {
    if root.join("index.html").is_file() {
        return Ok(());
    }
    let mut dirs = Vec::new();
    let mut loose_files = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        // file_type does not follow links: a symlinked "wrapper"
        // directory counts as a loose file so its target is never
        // renamed out from under the source.
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        } else if !entry.file_name().to_string_lossy().starts_with('.') {
            loose_files += 1;
        }
    }
    if dirs.len() == 1 && loose_files == 0 && dirs[0].join("index.html").is_file() {
        let wrapper = dirs.into_iter().next().unwrap();
        for entry in std::fs::read_dir(&wrapper)? {
            let entry = entry?;
            std::fs::rename(entry.path(), root.join(entry.file_name()))?;
        }
        std::fs::remove_dir(&wrapper)?;
    }
    Ok(())
}

/// Tree invariants every install source must satisfy after
/// normalization: `index.html` at the root, no symlinks anywhere,
/// total size under `frontends.max_uncompressed_size`.
pub fn validate_tree(config: &Config, root: &Path) -> Result<()> {
    if !root.join("index.html").is_file() {
        return Err(Error::Validation(
            "frontend must contain an index.html".into(),
        ));
    }
    let mut stack = vec![root.to_path_buf()];
    let mut total = 0u64;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if entry.file_type()?.is_symlink() {
                return Err(Error::Validation(format!(
                    "symlinks are not allowed in frontends ({})",
                    entry.path().display()
                )));
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
                if total > config.frontends.max_uncompressed_size {
                    return Err(Error::Validation(format!(
                        "frontend exceeds the uncompressed size cap of {} bytes",
                        config.frontends.max_uncompressed_size
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Install a frontend from zip bytes (admin upload).
pub async fn install_from_zip(
    config: &Config,
    name: &str,
    version: Option<&str>,
    bytes: &[u8],
) -> Result<Frontend> {
    validate_name(name)?;
    ensure_free(config, name)?;

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| Error::Validation(format!("not a zip archive: {e}")))?;

    // Headers-only pass: path safety, symlinks and the cumulative
    // uncompressed cap all fail the request before extraction.
    let mut total = 0u64;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| Error::Validation(format!("bad archive entry #{i}: {e}")))?;
        if entry.enclosed_name().is_none() {
            return Err(Error::Validation(format!(
                "archive entry {:?} escapes the frontend root",
                entry.name()
            )));
        }
        const S_IFMT: u32 = 0o170000;
        const S_IFLNK: u32 = 0o120000;
        if entry.unix_mode().is_some_and(|m| m & S_IFMT == S_IFLNK) {
            return Err(Error::Validation(format!(
                "symlinks are not allowed in frontend archives ({})",
                entry.name()
            )));
        }
        total += entry.size();
        if total > config.frontends.max_uncompressed_size {
            return Err(Error::Validation(format!(
                "archive exceeds the uncompressed size cap of {} bytes",
                config.frontends.max_uncompressed_size
            )));
        }
    }

    let staging = staging_dir(config, name)?;
    let result = (|| -> Result<()> {
        let mut written = 0u64;
        for i in 0..archive.len() {
            let entry = archive
                .by_index(i)
                .map_err(|e| Error::Validation(format!("bad archive entry #{i}: {e}")))?;
            let Some(path) = entry.enclosed_name() else {
                continue; // already rejected in pass 1
            };
            let target = staging.join(path);
            if entry.is_dir() {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // The read stays bounded per entry, and a cumulative
            // counter enforces the cap on *actual* bytes — a lying
            // header cannot under-report its way past it.
            use std::io::Read;
            let mut file = std::fs::File::create(&target)?;
            written += std::io::copy(
                &mut entry.take(config.frontends.max_uncompressed_size.saturating_add(1)),
                &mut file,
            )?;
            if written > config.frontends.max_uncompressed_size {
                return Err(Error::Validation(format!(
                    "archive exceeds the uncompressed size cap of {} bytes",
                    config.frontends.max_uncompressed_size
                )));
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            let finish = normalize(&staging)
                .and_then(|_| validate_tree(config, &staging))
                .and_then(|_| write_manifest(&staging, name, version, "zip"))
                .and_then(|_| promote(config, name, &staging));
            if finish.is_err() {
                cleanup(&staging);
            }
            finish
        }
        Err(e) => {
            cleanup(&staging);
            Err(e)
        }
    }
}

/// Install a frontend from a directory on the server filesystem.
/// Only active when `frontends.install_from_path_root` is set — the
/// source is canonicalized and must stay inside that root.
pub async fn install_from_path(
    config: &Config,
    name: &str,
    version: Option<&str>,
    path: &str,
) -> Result<Frontend> {
    validate_name(name)?;
    ensure_free(config, name)?;

    let Some(root) = &config.frontends.install_from_path_root else {
        return Err(Error::Validation(
            "filesystem frontend installs are disabled (frontends.install_from_path_root)".into(),
        ));
    };
    let root = std::fs::canonicalize(root)
        .map_err(|_| Error::Validation("install_from_path_root does not exist".into()))?;
    let source = std::fs::canonicalize(path)
        .map_err(|_| Error::Validation(format!("frontend path {path:?} does not exist")))?;
    if !source.starts_with(&root) {
        return Err(Error::Validation(format!(
            "frontend path {path:?} must be inside install_from_path_root"
        )));
    }

    let staging = staging_dir(config, name)?;
    let mut copied = 0u64;
    let result = copy_tree(&source, &staging, &mut copied, config)
        .and_then(|_| normalize(&staging))
        .and_then(|_| validate_tree(config, &staging))
        .and_then(|_| write_manifest(&staging, name, version, "path"))
        .and_then(|_| promote(config, name, &staging));
    if result.is_err() {
        cleanup(&staging);
    }
    result
}

/// Recursive copy — files only; a symlink in the source is a hard
/// validation error rather than a followed link. `written` accumulates
/// actual bytes copied so the uncompressed cap bites during the copy,
/// not only in `validate_tree` afterwards.
fn copy_tree(from: &Path, to: &Path, written: &mut u64, config: &Config) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_symlink() {
            return Err(Error::Validation(format!(
                "symlinks are not allowed in frontends ({})",
                entry.path().display()
            )));
        }
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target, written, config)?;
        } else {
            *written += std::fs::copy(entry.path(), &target)?;
            if *written > config.frontends.max_uncompressed_size {
                return Err(Error::Validation(format!(
                    "frontend exceeds the uncompressed size cap of {} bytes",
                    config.frontends.max_uncompressed_size
                )));
            }
        }
    }
    Ok(())
}

/// Install a frontend from a Git repository — shallow clone of an
/// optional ref, then the same tree validation as a zip.
///
/// The operation shells out to `git`; URLs are limited to `https:`/
/// `ssh:` so a local path or `file://` URL cannot smuggle server files
/// into the registry, and hooks/prompts are disabled so the clone
/// cannot execute attacker-controlled configuration or stall.
pub async fn install_from_git(
    config: &Config,
    name: &str,
    version: Option<&str>,
    url: &str,
    r#ref: Option<&str>,
) -> Result<Frontend> {
    validate_name(name)?;
    ensure_free(config, name)?;

    if !config.frontends.git_enabled {
        return Err(Error::Validation(
            "git frontend installs are disabled (frontends.git_enabled)".into(),
        ));
    }
    if !url.starts_with("https://") && !url.starts_with("ssh://") && !url.starts_with("git@") {
        return Err(Error::Validation(
            "git frontend installs accept only https://, ssh:// or scp-style URLs".into(),
        ));
    }
    // Defense in depth: option-like refs and hosts must never reach
    // the git command line. Git itself guards `host` starting with
    // '-' (CVE-2017-1000117), and argv separation protects `--branch`
    // — reject them explicitly anyway.
    if r#ref.is_some_and(|r| r.starts_with('-')) {
        return Err(Error::Validation("git ref must not start with '-'".into()));
    }
    // `scheme://[user@]host[:port][/path]` or scp-style `[user@]host:path`.
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split(['/', ':']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if host.is_empty() || host.starts_with('-') {
        return Err(Error::Validation("invalid git URL".into()));
    }

    let staging = staging_dir(config, name)?;
    let mut args: Vec<std::ffi::OsString> = vec![
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "clone".into(),
        "--depth".into(),
        "1".into(),
    ];
    if let Some(r) = r#ref {
        args.push("--branch".into());
        args.push(r.into());
    }
    args.push(url.into());
    args.push(staging.clone().into_os_string());

    let outcome = tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("git")
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            // BatchMode keeps the ssh child from blocking on host-key
            // or password prompts, which `GIT_TERMINAL_PROMPT` alone
            // does not cover. `accept-new` is trust-on-first-use: the
            // first key a host presents is recorded in the server
            // user's known_hosts — an accepted trade-off for unattended
            // installs; a changed key still fails the clone.
            .env(
                "GIT_SSH_COMMAND",
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
            )
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            // A system-level `url.<base>.insteadOf` rewrite could turn
            // an allowed https:// URL into file:/// and bypass the
            // scheme allowlist.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            // Git enforces the same transport allowlist as the URL check
            // above, including for redirects and rewritten URLs.
            .env("GIT_ALLOW_PROTOCOL", "https:ssh")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        // A stalled remote (TCP open, never answers) would hold the
        // blocking-pool thread — and the admin request — forever, so
        // poll with a deadline and kill on expiry.
        let deadline = std::time::Instant::now() + GIT_CLONE_TIMEOUT;
        loop {
            match child.try_wait()? {
                Some(_) => return child.wait_with_output().map(Some),
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait(); // reap
                    return Ok(None);
                }
                None => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
    })
    .await
    .map_err(|e| Error::Internal(format!("git task join failed: {e}")))?
    .map_err(|e| Error::Internal(format!("git is not available: {e}")))?;

    let Some(outcome) = outcome else {
        cleanup(&staging);
        return Err(Error::Validation("git clone timed out".into()));
    };

    if !outcome.status.success() {
        cleanup(&staging);
        tracing::warn!(
            status = %outcome.status,
            stderr = %String::from_utf8_lossy(&outcome.stderr),
            "frontend git clone failed"
        );
        return Err(Error::Validation(format!(
            "git clone failed with status {}",
            outcome.status
        )));
    }

    let result = (|| -> Result<()> {
        // The clone's own history must never ship as frontend content.
        let dotgit = staging.join(".git");
        if dotgit.exists() {
            std::fs::remove_dir_all(&dotgit)?;
        }
        normalize(&staging)?;
        validate_tree(config, &staging)?;
        write_manifest(&staging, name, version, "git")
    })()
    .and_then(|_| promote(config, name, &staging));
    if result.is_err() {
        cleanup(&staging);
    }
    result
}

/// Remove an installed frontend. The builtin and the admin default
/// refuse removal; per-user references do not block — they fall back
/// to the default at read time.
pub async fn remove(config: &Config, name: &str) -> Result<()> {
    // The route param is arbitrary text — without validation a
    // decoded `%2E%2E` would join `frontends.dir`'s parent into a
    // recursive delete.
    validate_name(name)?;
    if name == BUILTIN {
        return Err(Error::Validation(
            "the builtin frontend cannot be removed".into(),
        ));
    }
    if name == config.admin.default_frontend {
        return Err(Error::Validation(
            "the default frontend cannot be removed".into(),
        ));
    }
    let dir = Path::new(&config.frontends.dir).join(name);
    // Same rule as `find()`: registry entries are real directories.
    if !dir.is_dir() || dir.is_symlink() {
        return Err(Error::NotFound(format!(
            "frontend {name:?} is not installed"
        )));
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontends::find;
    use crate::frontends::testutil::{config_in, make_zip, temp_dir};

    #[tokio::test]
    async fn zip_install_happy_path() {
        let dir = temp_dir();
        let config = config_in(&dir);
        let zip = make_zip(&[
            ("index.html", b"<html>ok</html>" as &[u8]),
            ("assets/app.js", b"console.log(1)"),
        ]);
        let fe = install_from_zip(&config, "alt", Some("1.0"), &zip)
            .await
            .unwrap();
        assert_eq!(fe.name, "alt");
        assert_eq!(fe.manifest.source.as_deref(), Some("zip"));
        assert!(fe.root.join("index.html").is_file());
        assert!(fe.root.join("assets/app.js").is_file());
        assert!(find(&config, "alt").unwrap().is_some());
    }

    #[tokio::test]
    async fn zip_install_hoists_wrapper_directory() {
        let dir = temp_dir();
        let config = config_in(&dir);
        let zip = make_zip(&[
            ("dist/index.html", b"<html>ok</html>" as &[u8]),
            ("dist/a.css", b"body{}"),
        ]);
        let fe = install_from_zip(&config, "wrapped", None, &zip)
            .await
            .unwrap();
        assert!(fe.root.join("index.html").is_file());
        assert!(fe.root.join("a.css").is_file());
    }

    #[tokio::test]
    async fn zip_install_rejects_traversal_and_missing_index() {
        let dir = temp_dir();
        let config = config_in(&dir);

        let evil = make_zip(&[("../evil.txt", b"x" as &[u8])]);
        install_from_zip(&config, "evil", None, &evil)
            .await
            .unwrap_err();

        let empty = make_zip(&[("readme.txt", b"hi" as &[u8])]);
        install_from_zip(&config, "noindex", None, &empty)
            .await
            .unwrap_err();

        assert!(find(&config, "evil").unwrap().is_none());
        assert!(find(&config, "noindex").unwrap().is_none());
    }

    #[tokio::test]
    async fn zip_install_conflicts_and_size_caps() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        config.frontends.max_uncompressed_size = 64;
        let zip = make_zip(&[("index.html", &b"aaaa".repeat(64))]);
        install_from_zip(&config, "big", None, &zip)
            .await
            .unwrap_err();

        config.frontends.max_uncompressed_size = u64::MAX;
        let zip = make_zip(&[("index.html", b"ok" as &[u8])]);
        install_from_zip(&config, "taken", None, &zip)
            .await
            .unwrap();
        let err = install_from_zip(&config, "taken", None, &zip)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)));
        install_from_zip(&config, "yonder", None, &zip)
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn path_install_confined_to_root() {
        let dir = temp_dir();
        let root = dir.join("src-root");
        std::fs::create_dir_all(root.join("alt")).unwrap();
        std::fs::write(root.join("alt/index.html"), b"ok").unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("index.html"), b"nope").unwrap();

        let mut config = config_in(&dir);
        install_from_path(&config, "alt", None, root.join("alt").to_str().unwrap())
            .await
            .unwrap_err(); // disabled without install_from_path_root

        config.frontends.install_from_path_root = Some(root.to_string_lossy().into());
        let fe = install_from_path(&config, "alt", None, root.join("alt").to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(fe.manifest.source.as_deref(), Some("path"));
        install_from_path(&config, "esc", None, outside.to_str().unwrap())
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn git_install_gate_and_scheme() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        install_from_git(&config, "g", None, "https://x/r", None)
            .await
            .unwrap_err(); // disabled
        config.frontends.git_enabled = true;
        install_from_git(&config, "g", None, "file:///etc/passwd", None)
            .await
            .unwrap_err();
        install_from_git(&config, "g", None, "git://x/r", None)
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn git_install_rejects_option_like_refs_and_hosts() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        config.frontends.git_enabled = true;
        for bad in [
            "ssh://-oProxyCommand=x/r",
            "git@-oProxyCommand=x:r",
            "https://-x/r",
            "ssh:///path",
        ] {
            install_from_git(&config, "g", None, bad, None)
                .await
                .unwrap_err();
        }
        install_from_git(&config, "g", None, "https://x/r", Some("--upload-pack=/x"))
            .await
            .unwrap_err();
    }

    /// Full git pipeline over the network — clone, `.git` removal,
    /// normalization, validation, promotion. `#[ignore]`d so CI never
    /// depends on a third-party host. Run explicitly with
    /// `YONDER_TEST_GIT_URL=<https or ssh URL of a repo with index.html at
    /// its root> cargo test --lib -- --ignored frontends`; without the
    /// variable the test returns early.
    #[tokio::test]
    #[ignore = "clones a real repository over the network"]
    async fn git_install_happy_path() {
        let Ok(url) = std::env::var("YONDER_TEST_GIT_URL") else {
            return;
        };
        let dir = temp_dir();
        let mut config = config_in(&dir);
        config.frontends.git_enabled = true;
        let fe = install_from_git(&config, "hello", None, &url, None)
            .await
            .unwrap();
        assert_eq!(fe.manifest.source.as_deref(), Some("git"));
        assert!(fe.root.join("index.html").is_file());
        assert!(!fe.root.join(".git").exists());
        assert!(find(&config, "hello").unwrap().is_some());
    }

    #[tokio::test]
    async fn path_install_caps_total_size_during_copy() {
        let dir = temp_dir();
        let root = dir.join("src-root");
        std::fs::create_dir_all(root.join("big")).unwrap();
        std::fs::write(root.join("big/index.html"), b"ok").unwrap();
        std::fs::write(root.join("big/app.js"), vec![b'a'; 256]).unwrap();
        let mut config = config_in(&dir);
        config.frontends.install_from_path_root = Some(root.to_string_lossy().into());
        config.frontends.max_uncompressed_size = 64;
        install_from_path(&config, "big", None, root.join("big").to_str().unwrap())
            .await
            .unwrap_err();
        assert!(find(&config, "big").unwrap().is_none());
    }

    /// Rewrite the uncompressed-size fields of a `make_zip` archive
    /// so the headers under-report the real content — the shape of a
    /// crafted zip bomb the headers-only pass cannot see.
    fn with_lying_sizes(zip: &[u8], pretend: u32) -> Vec<u8> {
        let mut out = zip.to_vec();
        assert_eq!(&out[0..4], b"PK\x03\x04");
        out[22..26].copy_from_slice(&pretend.to_le_bytes()); // local header
        let cd = out.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        out[cd + 24..cd + 28].copy_from_slice(&pretend.to_le_bytes()); // central dir
        out
    }

    #[tokio::test]
    async fn zip_install_caps_actual_bytes_written() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        config.frontends.max_uncompressed_size = 64;
        // Declared as 4 bytes but extracts to 256 — pass 1 trusts the
        // header, so the cumulative extraction counter must stop it.
        let zip = with_lying_sizes(&make_zip(&[("index.html", &b"a".repeat(256))]), 4);
        install_from_zip(&config, "bomb", None, &zip)
            .await
            .unwrap_err();
        assert!(find(&config, "bomb").unwrap().is_none());
        assert_eq!(std::fs::read_dir(&config.frontends.dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn remove_rejects_traversal_names() {
        let dir = temp_dir();
        let config = config_in(&dir);
        let sentinel = dir.join("keep.txt");
        std::fs::write(&sentinel, b"safe").unwrap();
        for name in ["..", "../..", "a/b", ".hidden", "UPPER"] {
            remove(&config, name).await.unwrap_err();
        }
        assert!(sentinel.is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_does_not_follow_symlinks() {
        let dir = temp_dir();
        let config = config_in(&dir);
        let outside = dir.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"safe").unwrap();
        std::fs::create_dir_all(dir.join("frontends")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("frontends/linked")).unwrap();

        remove(&config, "linked").await.unwrap_err();
        assert!(outside.join("keep.txt").is_file());
    }

    #[tokio::test]
    async fn remove_refuses_builtin_and_default() {
        let dir = temp_dir();
        let mut config = config_in(&dir);
        let zip = make_zip(&[("index.html", b"ok" as &[u8])]);
        install_from_zip(&config, "alt", None, &zip).await.unwrap();

        remove(&config, "yonder").await.unwrap_err();
        config.admin.default_frontend = "alt".to_string();
        remove(&config, "alt").await.unwrap_err();
        config.admin.default_frontend = "yonder".to_string();
        remove(&config, "alt").await.unwrap();
        assert!(find(&config, "alt").unwrap().is_none());
        remove(&config, "alt").await.unwrap_err();
    }
}
