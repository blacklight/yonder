//! Status media attachments — upload processing and storage (epic 12).
//!
//! [`store_status_media`] is the single storage seam epic 18's
//! `local`/`s3` backend plugs into: it writes through
//! [`storage::write_bytes`] under `media_attachments/{account}/{uuid}.{ext}`
//! (the prefix epic 11's `delete_account_files` already cleans) and
//! persists a `media_attachments` row with `status_id` NULL until the
//! post attaches it. Images go through the shared
//! [`images::process_image`] pipeline (sniff → bounded decode → EXIF
//! strip → blurhash, `processed = true`); audio/video store raw — no
//! transcoding or video blurhashes this epic.

use crate::{config::Config, Error, Result};

/// Storage prefix — matches the `media_attachments` table name and the
/// prefix `storage::delete_account_files` already removes.
pub const PREFIX: &str = "media_attachments";

/// Storage prefix for link-preview card images (`previews/{file}`).
pub const PREVIEWS_PREFIX: &str = "previews";

/// Raw-store audio/video types: `(extension, content_type, kind)`.
/// `document` stays legal in the schema but is not accepted from the
/// API this epic.
const RAW_TYPES: &[(&str, &str, &str)] = &[
    ("mp4", "video/mp4", "video"),
    ("webm", "video/webm", "video"),
    ("mov", "video/quicktime", "video"),
    ("m4v", "video/x-m4v", "video"),
    ("mp3", "audio/mpeg", "audio"),
    ("ogg", "audio/ogg", "audio"),
    ("oga", "audio/ogg", "audio"),
    ("opus", "audio/opus", "audio"),
    ("wav", "audio/wav", "audio"),
    ("flac", "audio/flac", "audio"),
    ("m4a", "audio/mp4", "audio"),
    ("aac", "audio/aac", "audio"),
];

/// MIME for an extension — extends [`images::content_type_for_key`]
/// to audio/video uploads.
pub fn content_type_for_ext(ext: &str) -> Option<&'static str> {
    RAW_TYPES
        .iter()
        .find(|(e, _, _)| *e == ext)
        .map(|(_, t, _)| *t)
}

/// `(kind, ext)` sniffed from magic bytes, or `None`. Common container
/// signatures only — anything else falls back to the declared MIME.
fn sniff_raw(bytes: &[u8]) -> Option<(&'static str, &'static str, &'static str)> {
    let head = bytes.get(..16).unwrap_or(bytes);
    let is = |sig: &[u8], at: usize| head.get(at..at + sig.len()) == Some(sig);
    if is(b"ftyp", 4) {
        // ISOBMFF family — brand decides.
        if is(b"M4A", 8) {
            return Some(("m4a", "audio/mp4", "audio"));
        }
        if is(b"M4V", 8) {
            return Some(("m4v", "video/x-m4v", "video"));
        }
        if is(b"qt", 8) {
            return Some(("mov", "video/quicktime", "video"));
        }
        return Some(("mp4", "video/mp4", "video"));
    }
    if is(b"\x1A\x45\xDF\xA3", 0) {
        return Some(("webm", "video/webm", "video"));
    }
    if is(b"ID3", 0) || is(b"\xFF\xFB", 0) || is(b"\xFF\xFA", 0) || is(b"\xFF\xF3", 0) {
        return Some(("mp3", "audio/mpeg", "audio"));
    }
    if is(b"OggS", 0) {
        return Some(("ogg", "audio/ogg", "audio"));
    }
    if is(b"RIFF", 0) && is(b"WAVE", 8) {
        return Some(("wav", "audio/wav", "audio"));
    }
    if is(b"fLaC", 0) {
        return Some(("flac", "audio/flac", "audio"));
    }
    None
}

/// Map a declared `audio/*`/`video/*` MIME to `(ext, mime, kind)` —
/// unknown subtypes get a per-top-level fallback extension.
fn for_declared(mime: &str) -> Option<(String, String, &'static str)> {
    let mime = mime.split(';').next().unwrap_or("").trim();
    if let Some(&(ext, m, k)) = RAW_TYPES.iter().find(|(_, m, _)| *m == mime) {
        return Some((ext.to_string(), m.to_string(), k));
    }
    if let Some(sub) = mime.strip_prefix("video/") {
        if !sub.is_empty()
            && sub
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '+')
        {
            return Some(("mp4".to_string(), mime.to_string(), "video"));
        }
    }
    if let Some(sub) = mime.strip_prefix("audio/") {
        if !sub.is_empty()
            && sub
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '+')
        {
            return Some(("m4a".to_string(), mime.to_string(), "audio"));
        }
    }
    None
}

/// A stored status-media attachment (the `media_attachments` row).
pub struct StoredAttachment {
    /// `media_attachments.id`.
    pub id: i64,
    /// Storage key written under [`PREFIX`].
    pub key: String,
    /// `image|video|audio`.
    pub kind: String,
    /// Detected MIME type.
    pub content_type: String,
    /// Stored byte count.
    pub file_size: i64,
}

/// Upload parameters beyond the file bytes.
#[derive(Debug, Default)]
pub struct UploadMeta {
    /// Alt text.
    pub description: Option<String>,
    /// Focal point `(x, y)`, each in `[-1, 1]`.
    pub focus: Option<(f32, f32)>,
    /// Declared `Content-Type` of the multipart part (advisory — the
    /// real type is sniffed for images and magic-checked for A/V).
    pub declared_content_type: Option<String>,
}

/// Validate `focus` coordinates — each in `[-1, 1]` or absent.
pub fn validate_focus(focus: Option<(f32, f32)>) -> Result<Option<(f32, f32)>> {
    match focus {
        Some((x, y)) if !(-1.0..=1.0).contains(&x) || !(-1.0..=1.0).contains(&y) => Err(
            Error::Validation("focus coordinates must be in [-1, 1]".to_string()),
        ),
        other => Ok(other),
    }
}

/// Parse a `focus` form/JSON value — `"x,y"` or a `{x, y}` object.
pub fn parse_focus(value: Option<&str>) -> Result<Option<(f32, f32)>> {
    let Some(value) = value else { return Ok(None) };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let (x, y) = trimmed
        .split_once(',')
        .ok_or_else(|| Error::Validation("focus must be \"x,y\"".to_string()))?;
    let x: f32 = x
        .trim()
        .parse()
        .map_err(|_| Error::Validation("focus x must be a number".to_string()))?;
    let y: f32 = y
        .trim()
        .parse()
        .map_err(|_| Error::Validation("focus y must be a number".to_string()))?;
    Ok(Some((x, y)))
}

/// Validate, process and persist an upload under
/// `media_attachments/{account_id}/{uuid}.{ext}`; insert the
/// `media_attachments` row (`status_id` NULL). Images get the full
/// pipeline (blurhash, dimensions, conditional EXIF strip,
/// `processed = true`); audio/video store raw with
/// `processed = false` — no transcoding this epic.
pub async fn store_status_media(
    config: &Config,
    db: &sqlx::PgPool,
    account_id: i64,
    bytes: Vec<u8>,
    meta: UploadMeta,
) -> Result<StoredAttachment> {
    let max = config.media.max_upload_size;
    if bytes.is_empty() {
        return Err(Error::Validation("media upload is empty".to_string()));
    }
    if bytes.len() as u64 > max {
        return Err(Error::Validation(format!(
            "media is too large ({} bytes, at most {max})",
            bytes.len()
        )));
    }
    if let Some(desc) = &meta.description {
        if desc.chars().count() > 5000 {
            return Err(Error::Validation(
                "media description must be at most 5000 characters".to_string(),
            ));
        }
    }
    let focus = validate_focus(meta.focus)?;

    // Image? The sniffer decides — a `video/*` label on real PNG bytes
    // still lands as an image.
    let is_image = image::guess_format(&bytes)
        .map(|f| {
            matches!(
                f,
                image::ImageFormat::Png
                    | image::ImageFormat::Jpeg
                    | image::ImageFormat::WebP
                    | image::ImageFormat::Gif
            )
        })
        .unwrap_or(false);

    let (stored, kind, content_type, ext, width, height, blurhash, processed, exif_stripped) =
        if is_image {
            let strip = config.media.strip_exif;
            let raw = bytes;
            let image = tokio::task::spawn_blocking(move || {
                super::images::process_image(&raw, strip, "media")
            })
            .await
            .map_err(|e| Error::Internal(format!("image task failed: {e}")))??;
            // GIFs are never re-encoded, so `strip` didn't apply.
            let stripped = strip && image.ext != "gif";
            (
                image.bytes,
                "image".to_string(),
                image.content_type,
                image.ext.to_string(),
                Some(image.width as i32),
                Some(image.height as i32),
                Some(image.blurhash),
                true,
                stripped,
            )
        } else {
            let (ext, mime, kind) = sniff_raw(&bytes)
                .map(|(e, m, k)| (e.to_string(), m.to_string(), k))
                .or_else(|| meta.declared_content_type.as_deref().and_then(for_declared))
                .ok_or_else(|| {
                    Error::Validation("media must be an image, video or audio upload".to_string())
                })?;
            (
                bytes,
                kind.to_string(),
                mime,
                ext,
                None,
                None,
                None,
                false,
                false,
            )
        };

    let key = format!("{PREFIX}/{account_id}/{}.{}", uuid::Uuid::new_v4(), ext);
    super::storage::write_bytes(config, &key, &stored).await?;

    let size = stored.len() as i64;
    let id = match sqlx::query_scalar::<_, i64>(
        "INSERT INTO media_attachments \
             (account_id, type, file_path, content_type, file_size, description, \
              blurhash, width, height, exif_stripped, processed, focus_x, focus_y) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
    )
    .bind(account_id)
    .bind(&kind)
    .bind(&key)
    .bind(&content_type)
    .bind(size)
    .bind(&meta.description)
    .bind(&blurhash)
    .bind(width)
    .bind(height)
    .bind(exif_stripped)
    .bind(processed)
    .bind(focus.map(|(x, _)| x))
    .bind(focus.map(|(_, y)| y))
    .fetch_one(db)
    .await
    {
        Ok(id) => id,
        Err(e) => {
            // Best-effort cleanup of the written object.
            let _ = super::storage::delete_key(config, &key).await;
            return Err(Error::Database(e));
        }
    };
    Ok(StoredAttachment {
        id,
        key,
        kind,
        content_type,
        file_size: size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmff(brand: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0, 0, 0, 0];
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(brand);
        b.extend_from_slice(&[0; 8]);
        b
    }

    #[test]
    fn isobmff_brands_classify() {
        assert_eq!(sniff_raw(&bmff(b"M4V ")).unwrap().2, "video");
        assert_eq!(sniff_raw(&bmff(b"M4A ")).unwrap().2, "audio");
        assert_eq!(sniff_raw(&bmff(b"isom")).unwrap().2, "video");
        assert_eq!(sniff_raw(&bmff(b"qt  ")).unwrap().0, "mov");
    }

    #[test]
    fn focus_bounds() {
        assert!(validate_focus(Some((0.5, -1.0))).is_ok());
        assert!(validate_focus(Some((1.5, 0.0))).is_err());
        assert!(validate_focus(Some((f32::NAN, 0.0))).is_err());
        assert!(parse_focus(Some("0.5,-0.5")).unwrap() == Some((0.5, -0.5)));
        assert!(parse_focus(Some("nope")).is_err());
    }
}
