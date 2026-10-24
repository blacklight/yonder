//! Profile-image pipeline: sniff → bounded decode → EXIF strip →
//! blurhash → write under `media.local_path`.
//!
//! The declared `Content-Type` of an upload is advisory only — the real
//! format is sniffed from the bytes. Decode is bounded (pixel and
//! allocation limits) so a small file can't expand into a
//! decompression bomb. Re-encoding through `image` drops EXIF by
//! construction; GIFs pass through untouched to preserve animation
//! (GIF carries no EXIF — comment/XMP extension blocks are a known
//! gap, see `docs/profile.md`).

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::{config::Config, Error, Result};

/// Decode bounds: `media.max_avatar_size` caps bytes, these cap what
/// the decoder may expand them into.
const MAX_DIMENSION: u32 = 4096;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;

/// The profile slot an upload is for — selects the storage prefix and
/// the account columns it lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// `accounts.avatar_path` — stored under `avatars/`.
    Avatar,
    /// `accounts.banner_path` — stored under `headers/`.
    Header,
    /// `custom_emojis.image_path` — stored under `emoji/`. The key
    /// keeps the uploader's account segment for namespacing, but the
    /// row is instance-owned: `delete_account_files` intentionally
    /// does not list the `emoji` prefix.
    Emoji,
}

impl MediaKind {
    /// Storage prefix the key is written under (and the `/media/`
    /// serving route's allow-list).
    pub fn dir(&self) -> &'static str {
        match self {
            MediaKind::Avatar => "avatars",
            MediaKind::Header => "headers",
            MediaKind::Emoji => "emoji",
        }
    }

    /// Client-facing name for error messages.
    pub fn label(&self) -> &'static str {
        match self {
            MediaKind::Avatar => "avatar",
            MediaKind::Header => "header",
            MediaKind::Emoji => "emoji",
        }
    }
}

/// What [`store_profile_image`] persisted — enough to update the
/// account row and to answer `GET /media/{key}`.
#[derive(Debug)]
pub struct StoredImage {
    /// Storage key (`{dir}/{account_id}/{uuid}.{ext}`) — this is what
    /// `avatar_path`/`banner_path` holds.
    pub key: String,
    /// MIME type for the serving route.
    pub content_type: String,
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// Blurhash placeholder of the first frame.
    pub blurhash: String,
}

const ALLOWED: &[(ImageFormat, &str, &str)] = &[
    (ImageFormat::Png, "png", "image/png"),
    (ImageFormat::Jpeg, "jpg", "image/jpeg"),
    (ImageFormat::WebP, "webp", "image/webp"),
    (ImageFormat::Gif, "gif", "image/gif"),
];

/// Sniff, bound-decode, conditionally strip EXIF and blurhash the
/// bytes. Pure CPU work — callers wrap it in `spawn_blocking`.
fn process(
    kind: MediaKind,
    bytes: &[u8],
    strip_exif: bool,
) -> Result<(Vec<u8>, String, StoredSeed)> {
    let format = image::guess_format(bytes).map_err(|_| {
        Error::Validation(format!(
            "{} must be an image (png/jpeg/webp/gif)",
            kind.label()
        ))
    })?;
    let Some(&(_, ext, content_type)) = ALLOWED.iter().find(|(f, _, _)| *f == format) else {
        return Err(Error::Validation(format!(
            "{} must be an image (png/jpeg/webp/gif)",
            kind.label()
        )));
    };

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Validation(format!("{} is not a readable image: {e}", kind.label())))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| {
        Error::Validation(format!(
            "{} could not be decoded (limits: {}x{}, {} MiB): {e}",
            kind.label(),
            MAX_DIMENSION,
            MAX_DIMENSION,
            MAX_DECODE_ALLOC / 1024 / 1024
        ))
    })?;
    let (width, height) = (decoded.width(), decoded.height());
    // Emoji don't need a blurhash placeholder — skip the encode.
    let blurhash = if kind == MediaKind::Emoji {
        String::new()
    } else {
        let rgba = decoded.to_rgba8();
        blurhash::encode(4, 3, width, height, rgba.as_raw())
            .map_err(|e| Error::Internal(format!("blurhash encode failed: {e}")))?
    };

    // GIF passthrough preserves animation; everything else is
    // re-encoded when EXIF stripping is on (the encoders write no
    // metadata), otherwise stored byte-identical.
    let bytes = if format == ImageFormat::Gif || !strip_exif {
        bytes.to_vec()
    } else {
        let mut out = Cursor::new(Vec::with_capacity(bytes.len()));
        decoded
            .write_to(&mut out, format)
            .map_err(|e| Error::Internal(format!("image re-encode failed: {e}")))?;
        out.into_inner()
    };
    Ok((
        bytes,
        content_type.to_string(),
        StoredSeed {
            ext,
            width,
            height,
            blurhash,
        },
    ))
}

struct StoredSeed {
    ext: &'static str,
    width: u32,
    height: u32,
    blurhash: String,
}

/// The processed output of [`process_image`] — shared by profile
/// uploads and status media (epic 12).
pub struct ProcessedImage {
    /// Stored bytes (re-encoded when EXIF stripping applies).
    pub bytes: Vec<u8>,
    /// Canonical MIME type for the sniffed format.
    pub content_type: String,
    /// Storage-key extension.
    pub ext: &'static str,
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// Blurhash placeholder of the first frame.
    pub blurhash: String,
}

/// The sniff → bound-decode → EXIF-strip → blurhash core, generalized
/// for status media. Pure CPU work — callers wrap it in
/// `spawn_blocking`. `label` names the upload kind in error messages.
pub(crate) fn process_image(bytes: &[u8], strip_exif: bool, label: &str) -> Result<ProcessedImage> {
    let format = image::guess_format(bytes)
        .map_err(|_| Error::Validation(format!("{label} must be an image (png/jpeg/webp/gif)")))?;
    let Some(&(_, ext, content_type)) = ALLOWED.iter().find(|(f, _, _)| *f == format) else {
        return Err(Error::Validation(format!(
            "{label} must be an image (png/jpeg/webp/gif)"
        )));
    };

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Validation(format!("{label} is not a readable image: {e}")))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|e| {
        Error::Validation(format!(
            "{label} could not be decoded (limits: {}x{}, {} MiB): {e}",
            MAX_DIMENSION,
            MAX_DIMENSION,
            MAX_DECODE_ALLOC / 1024 / 1024
        ))
    })?;
    let (width, height) = (decoded.width(), decoded.height());
    let rgba = decoded.to_rgba8();
    let blurhash = blurhash::encode(4, 3, width, height, rgba.as_raw())
        .map_err(|e| Error::Internal(format!("blurhash encode failed: {e}")))?;

    let bytes = if format == ImageFormat::Gif || !strip_exif {
        bytes.to_vec()
    } else {
        let mut out = Cursor::new(Vec::with_capacity(bytes.len()));
        decoded
            .write_to(&mut out, format)
            .map_err(|e| Error::Internal(format!("image re-encode failed: {e}")))?;
        out.into_inner()
    };
    Ok(ProcessedImage {
        bytes,
        content_type: content_type.to_string(),
        ext,
        width,
        height,
        blurhash,
    })
}

/// Validate, process and persist an upload; returns the storage key
/// and metadata. Files land under `media.local_path` (local backend)
/// or the S3 bucket at `{kind_dir}/{account_id}/{uuid}.{ext}`.
pub async fn store_profile_image(
    config: &Config,
    store: &crate::media::storage::MediaStore,
    kind: MediaKind,
    account_id: i64,
    bytes: Vec<u8>,
) -> Result<StoredImage> {
    let max = match kind {
        MediaKind::Emoji => config.media.max_emoji_size,
        _ => config.media.max_avatar_size,
    };
    if bytes.len() as u64 > max {
        return Err(Error::Validation(format!(
            "{} is too large ({} bytes, at most {})",
            kind.label(),
            bytes.len(),
            max
        )));
    }
    let strip_exif = config.media.strip_exif;
    let (bytes, content_type, seed) =
        tokio::task::spawn_blocking(move || process(kind, &bytes, strip_exif))
            .await
            .map_err(|e| Error::Internal(format!("image task failed: {e}")))??;

    let key = format!(
        "{}/{account_id}/{}.{}",
        kind.dir(),
        uuid::Uuid::new_v4(),
        seed.ext
    );
    store.put(&key, &bytes, &content_type).await?;
    Ok(StoredImage {
        key,
        content_type,
        width: seed.width,
        height: seed.height,
        blurhash: seed.blurhash,
    })
}

/// Content-type for a storage key's extension — the serving route's
/// allow-list, kept in lockstep with [`ALLOWED`] and the audio/video
/// table in [`crate::media::attachments`].
pub fn content_type_for_key(key: &str) -> Option<&'static str> {
    let ext = key.rsplit('.').next()?;
    ALLOWED
        .iter()
        .find(|(_, e, _)| *e == ext)
        .map(|(_, _, t)| *t)
        .or_else(|| super::attachments::content_type_for_ext(ext))
}
