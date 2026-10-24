//! Bulk emoji imports — zip archives and a configured server-side
//! directory (epic 20).
//!
//! Safety: zip entry names are sanitized like `media::attachments`
//! upload filenames (ASCII whitelist, no traversal), each entry is
//! size-capped before decode, a cumulative uncompressed cap and a
//! per-entry compression-ratio cap defeat zip bombs, and filesystem
//! imports are confined to the canonical `admin.emoji_import_dir`
//! root — paths outside it or symlinks escaping it are rejected.

use std::path::Path;

use sqlx::PgPool;

use crate::{config::Config, media::storage::MediaStore, Error, Result};

use super::model::{self, EmojiRow};

/// Allowed emoji image formats — a path's extension must be one of
/// these to be considered for import.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "avif"];

/// Per-entry compressed/uncompressed ratio cap.
const MAX_RATIO: u64 = 100;
/// Total entries processed per archive.
const MAX_ENTRIES: usize = 500;

/// Per-entry outcome from a bulk import.
#[derive(Debug)]
pub struct ImportOutcome {
    /// The zip path or filename as supplied.
    pub name: String,
    /// `Ok(row)` on success, `Err(message)` when the entry was
    /// skipped (bad name, oversize, decode failure, duplicate).
    pub result: std::result::Result<EmojiRow, String>,
}

/// Sanitize a shortcode derived from a filename stem — lowercase,
/// keep `[a-z0-9_]`, squeeze repeats. Returns `None` when nothing
/// valid survives.
pub fn shortcode_from_stem(stem: &str) -> Option<String> {
    let out: String = stem
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let squeezed = out
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if model::validate_shortcode(&squeezed).is_ok() {
        Some(squeezed)
    } else {
        None
    }
}

/// One decoded+stored emoji — shared by both import paths.
/// `overwrite` routes through [`model::replace_local`]: the
/// replacement is validated and stored *before* the existing row's
/// `image_path` swaps, and the old object is deleted only after a
/// successful swap — a bad file leaves the old emoji intact.
#[allow(clippy::too_many_arguments)]
async fn store_one(
    db: &PgPool,
    store: &MediaStore,
    config: &Config,
    shortcode: &str,
    bytes: Vec<u8>,
    uploader_id: i64,
    pack_id: Option<i64>,
    category: Option<&str>,
    overwrite: bool,
) -> std::result::Result<EmojiRow, String> {
    if overwrite {
        let (row, old_key) = model::replace_local(
            db,
            store,
            config,
            shortcode,
            bytes,
            uploader_id,
            pack_id,
            category,
        )
        .await
        .map_err(|e| e.to_string())?;
        if let Some(key) = old_key {
            if let Err(e) = store.delete(&key).await {
                tracing::warn!(key, error = %e, "replaced emoji object delete failed");
            }
        }
        return Ok(row);
    }
    model::create_local(
        db,
        store,
        config,
        shortcode,
        bytes,
        uploader_id,
        pack_id,
        category,
    )
    .await
    .map_err(|e| e.to_string())
}

/// How an archive entry is treated by the per-entry report.
enum ZipEntry {
    /// A usable image file — yields its bare filename.
    Accept(String),
    /// Not an emoji candidate (e.g. a non-image file) — reported
    /// as skipped so every file in the archive gets an outcome.
    Skip(&'static str),
    /// A name that violates path hygiene — reported as an error.
    Reject(&'static str),
}

/// Entry selection shared by both passes: `Some` for every file
/// entry (accepted, skipped, or refused), `None` only for directory
/// entries, which produce no report row.
fn zip_entry_kind(raw_name: &str, is_dir: bool) -> Option<ZipEntry> {
    if is_dir {
        return None;
    }
    // Path hygiene: flat files or one directory deep, no `..`,
    // no absolute paths (leading `/` or Windows-style `\`).
    if raw_name.starts_with('/') || raw_name.contains('\\') {
        return Some(ZipEntry::Reject("absolute paths are refused"));
    }
    let parts: Vec<&str> = raw_name.split('/').filter(|p| !p.is_empty()).collect();
    let filename = parts.last()?;
    if parts.contains(&"..") {
        return Some(ZipEntry::Reject("path traversal entries are refused"));
    }
    if parts.len() > 2 {
        return Some(ZipEntry::Reject(
            "entries nested deeper than one directory are refused",
        ));
    }
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Some(ZipEntry::Skip("skipped: not an image file"));
    }
    Some(ZipEntry::Accept((*filename).to_string()))
}

/// Import a zip archive: image entries at the archive root (or one
/// directory deep — nested dirs are ignored) become emoji whose
/// shortcode is the filename stem. `overwrite` replaces an existing
/// local shortcode row in place (image swapped, id preserved).
///
/// A first pass over the archive's central directory validates the
/// whole archive — entry count, per-entry compression ratio and the
/// cumulative uncompressed cap — and rejects the request (422)
/// *before any insert*. Pass two then reports per-entry outcomes for
/// read/decode/store failures. Header sizes are trusted for the
/// caps; actual reads stay bounded by `max_emoji_size` per entry.
pub async fn import_zip(
    db: &PgPool,
    store: &MediaStore,
    config: &Config,
    bytes: Vec<u8>,
    uploader_id: i64,
    pack_id: Option<i64>,
    overwrite: bool,
) -> Result<Vec<ImportOutcome>> {
    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(&bytes)) {
        Ok(a) => a,
        Err(e) => return Err(Error::Validation(format!("not a zip archive: {e}"))),
    };
    if archive.len() > MAX_ENTRIES {
        return Err(Error::Validation(format!(
            "archive has {} entries; at most {MAX_ENTRIES} are accepted",
            archive.len()
        )));
    }
    // Pass 1 — headers only. Any cap violation fails the request
    // before a single row is written.
    let max_uncompressed = config.media.max_emoji_zip_uncompressed;
    let mut total_uncompressed = 0u64;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        if !matches!(
            zip_entry_kind(entry.name(), entry.is_dir()),
            Some(ZipEntry::Accept(_))
        ) {
            continue;
        }
        if entry.compressed_size() > 0 && entry.size() / entry.compressed_size() > MAX_RATIO {
            return Err(Error::Validation(format!(
                "entry '{}' has a suspicious compression ratio",
                entry.name()
            )));
        }
        total_uncompressed += entry.size();
        if total_uncompressed > max_uncompressed {
            return Err(Error::Validation(format!(
                "archive exceeds the uncompressed size cap of {max_uncompressed} bytes"
            )));
        }
    }
    // Pass 2 — per-entry outcomes for everything that isn't an
    // archive-level rejection.
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(e) => {
                out.push(ImportOutcome {
                    name: format!("entry#{i}"),
                    result: Err(format!("bad entry: {e}")),
                });
                continue;
            }
        };
        let raw_name = entry.name().to_string();
        let Some(kind) = zip_entry_kind(&raw_name, entry.is_dir()) else {
            continue;
        };
        let filename = match kind {
            ZipEntry::Accept(f) => f,
            ZipEntry::Skip(reason) | ZipEntry::Reject(reason) => {
                out.push(ImportOutcome {
                    name: raw_name,
                    result: Err(reason.into()),
                });
                continue;
            }
        };
        let stem = filename
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(filename.as_str());
        let Some(shortcode) = shortcode_from_stem(stem) else {
            out.push(ImportOutcome {
                name: raw_name,
                result: Err("filename does not yield a valid shortcode".into()),
            });
            continue;
        };
        let mut buf = Vec::with_capacity(entry.size().min(1 << 22) as usize);
        use std::io::Read;
        if let Err(e) = entry
            .take(config.media.max_emoji_size + 1)
            .read_to_end(&mut buf)
        {
            out.push(ImportOutcome {
                name: raw_name,
                result: Err(format!("read failed: {e}")),
            });
            continue;
        }
        if buf.len() as u64 > config.media.max_emoji_size {
            out.push(ImportOutcome {
                name: raw_name,
                result: Err("entry exceeds the per-emoji size cap".into()),
            });
            continue;
        }
        let result = store_one(
            db,
            store,
            config,
            &shortcode,
            buf,
            uploader_id,
            pack_id,
            None,
            overwrite,
        )
        .await;
        out.push(ImportOutcome {
            name: raw_name,
            result,
        });
    }
    Ok(out)
}

/// One scanned directory file: `(name, Ok((shortcode, bytes)))` for
/// importable images, `(name, Err(reason))` for entries that must
/// appear in the outcome report.
type ScannedFile = (String, std::result::Result<(String, Vec<u8>), String>);

/// The synchronous half of [`from_dir`] — runs on the blocking
/// pool. Resolves `from_dir` under the canonical import root,
/// enumerates at most `MAX_ENTRIES` files, and for each image file
/// returns `(name, Ok((shortcode, bytes)))`; entries that must
/// appear in the report carry `Err(reason)`. File sizes are checked
/// via metadata first and the read itself is bounded by
/// `max_size + 1`, so a huge file never lands whole in memory.
fn scan_dir(import_root: &Path, from_dir: &str, max_size: u64) -> Result<Vec<ScannedFile>> {
    use std::io::Read;
    let root = std::fs::canonicalize(import_root)
        .map_err(|e| Error::Validation(format!("emoji import root unreadable: {e}")))?;
    let dir = std::fs::canonicalize(root.join(from_dir.trim_start_matches('/')))
        .map_err(|_| Error::NotFound("import directory".into()))?;
    if !dir.starts_with(&root) || !dir.is_dir() {
        return Err(Error::Validation(
            "import path escapes the emoji import root".to_string(),
        ));
    }
    let files: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|e| Error::Internal(format!("read_dir failed: {e}")))?
        .flatten()
        .filter(|e| e.path().is_file()) // non-recursive
        .take(MAX_ENTRIES + 1)
        .collect();
    if files.len() > MAX_ENTRIES {
        return Err(Error::Validation(format!(
            "directory holds more than {MAX_ENTRIES} files"
        )));
    }
    let mut out = Vec::new();
    for entry in files {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Re-canonicalize per file — a symlink inside the root must
        // not escape it.
        let Ok(canon) = std::fs::canonicalize(&path) else {
            continue;
        };
        if !canon.starts_with(&root) {
            out.push((name, Err("symlink escapes the import root".into())));
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !IMAGE_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(shortcode) = shortcode_from_stem(&stem) else {
            out.push((
                name,
                Err("filename does not yield a valid shortcode".into()),
            ));
            continue;
        };
        match std::fs::metadata(&canon) {
            Ok(m) if m.len() > max_size => {
                out.push((name, Err("file exceeds the per-emoji size cap".into())));
                continue;
            }
            Err(e) => {
                out.push((name, Err(format!("stat failed: {e}"))));
                continue;
            }
            _ => {}
        }
        // The metadata check may be stale by now — bound the read too.
        match std::fs::File::open(&canon).and_then(|f| {
            let mut buf = Vec::new();
            f.take(max_size + 1).read_to_end(&mut buf).map(|_| buf)
        }) {
            Ok(b) if b.len() as u64 > max_size => {
                out.push((name, Err("file exceeds the per-emoji size cap".into())));
            }
            Ok(b) => out.push((name, Ok((shortcode, b)))),
            Err(e) => out.push((name, Err(format!("read failed: {e}")))),
        }
    }
    Ok(out)
}

/// Import image files from a directory on the server filesystem.
/// `from_dir` is resolved against `config.admin.emoji_import_dir`
/// (the import root — unset means the endpoint is disabled): the
/// given path must stay inside it after canonicalization, and
/// symlinks escaping the root are refused. The scan and reads run
/// on the blocking pool; rows are written one at a time here.
pub async fn from_dir(
    db: &PgPool,
    store: &MediaStore,
    config: &Config,
    from_dir: &str,
    uploader_id: i64,
    pack_id: Option<i64>,
) -> Result<Vec<ImportOutcome>> {
    let root = config
        .admin
        .emoji_import_dir
        .as_ref()
        .ok_or_else(|| {
            Error::Validation(
                "filesystem emoji import is disabled (admin.emoji_import_dir unset)".into(),
            )
        })?
        .clone();
    let from_dir = from_dir.to_string();
    let max_size = config.media.max_emoji_size;
    let scanned =
        tokio::task::spawn_blocking(move || scan_dir(Path::new(&root), &from_dir, max_size))
            .await
            .map_err(|e| Error::Internal(format!("emoji import scan failed: {e}")))??;
    let mut out = Vec::new();
    for (name, entry) in scanned {
        let (shortcode, bytes) = match entry {
            Ok(v) => v,
            Err(msg) => {
                out.push(ImportOutcome {
                    name,
                    result: Err(msg),
                });
                continue;
            }
        };
        let result = store_one(
            db,
            store,
            config,
            &shortcode,
            bytes,
            uploader_id,
            pack_id,
            None,
            false,
        )
        .await;
        out.push(ImportOutcome { name, result });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_shortcodes() {
        assert_eq!(shortcode_from_stem("blob-cat!"), Some("blob_cat".into()));
        assert_eq!(shortcode_from_stem("ok_2"), Some("ok_2".into()));
        assert_eq!(shortcode_from_stem(".."), None);
        assert_eq!(shortcode_from_stem("a"), None); // too short
        assert_eq!(shortcode_from_stem("UPPER"), Some("upper".into()));
    }
}
