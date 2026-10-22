//! Media attachments: upload, processing, local/S3 storage.
//!
//! Epic 09 lands the local-backend storage contract epic 18 builds on:
//! uploads are written under `media.local_path` at
//! `{kind}/{account_id}/{uuid}.{ext}` and the *key* is stored on the
//! account row (`avatar_path`/`banner_path`) — [`public_url`] renders
//! the public URL at read time so a base-URL or CDN change doesn't
//! invalidate stored data. `GET /media/{*key}` serves the local
//! backend; the S3 backend is epic 18's.
//!
//! Epic 12 adds status media through [`attachments::store_status_media`]
//! under `media_attachments/{account}/{uuid}.{ext}` — the single seam
//! epic 18's `local|s3` backend and remote-media caching plug into.

pub mod attachments;
pub mod images;
pub mod storage;

pub use attachments::store_status_media;
pub use images::{store_profile_image, MediaKind, StoredImage};
pub use storage::public_url;
