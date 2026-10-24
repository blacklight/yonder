//! Storage backends: key ↔ public URL mapping, the `MediaStore`
//! interface over `local`/`s3`, and the `GET /media/{*key}` serving
//! route.
//!
//! Storage keys live under `media.local_path` (local backend) or the
//! configured S3 bucket, and are the value stored in `avatar_path`/
//! `banner_path`/`file_path` — [`public_url`] renders the absolute URL
//! at read time, so `federation.base_url` or a CDN change doesn't
//! invalidate stored data.
//!
//! One [`MediaStore`] is built per process and shared through
//! `AppState::media`, `JobContext::media` and `PostContext::media` —
//! every read/write/delete of stored bytes goes through it, so the
//! backends are interchangeable behind the same key space.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::config::{Config, MediaBackend};
use crate::http::AppState;
use crate::{Error, Result};

/// The absolute URL clients and the actor document (`icon_url`) see
/// for a stored key. `/{base}/media/{key}` normally; when the S3
/// backend has `media.s3_public_base_url` configured, media is linked
/// from the CDN directly (`{cdn}/{key}`).
pub fn public_url(config: &Config, base_url: &str, key: &str) -> String {
    if matches!(config.media.backend, MediaBackend::S3) {
        if let Some(cdn) = config.media.s3_public_base_url.as_deref() {
            return format!("{}/{}", cdn.trim_end_matches('/'), key);
        }
    }
    format!("{}/media/{}", base_url.trim_end_matches('/'), key)
}

/// One stored object, returned by [`MediaStore::list_prefix`].
pub struct StoredObject {
    /// Storage key relative to the backend root.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
}

/// Backend-agnostic media store — dispatch over the configured
/// `media.backend`. Construct once via [`MediaStore::from_config`] and
/// clone freely (cheap).
#[derive(Clone)]
pub enum MediaStore {
    /// Local filesystem under `media.local_path`.
    Local(LocalStore),
    /// S3-compatible object store (Section 4 of epic 18).
    S3(S3Store),
}

impl MediaStore {
    /// Build the store for `config.media.backend`. Panics at startup on
    /// a grossly misconfigured S3 backend (missing bucket/credentials)
    /// — the store exists for the process lifetime, so a bad deploy
    /// must not start.
    pub fn from_config(config: &Config) -> Self {
        match config.media.backend {
            MediaBackend::Local => MediaStore::Local(LocalStore {
                root: PathBuf::from(&config.media.local_path),
            }),
            MediaBackend::S3 => MediaStore::S3(S3Store::from_config(config)),
        }
    }

    /// Write `bytes` to `key`, creating parents on the local backend.
    /// `content_type` is recorded by object stores; the local serving
    /// route re-derives it from the key's extension.
    pub async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<()> {
        match self {
            MediaStore::Local(s) => s.put(key, bytes).await,
            MediaStore::S3(s) => s.put(key, bytes, content_type).await,
        }
    }

    /// Read `key`. Missing keys map to `Error::NotFound`.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            MediaStore::Local(s) => s.get(key).await,
            MediaStore::S3(s) => s.get(key).await,
        }
    }

    /// Remove `key` if it exists (idempotent).
    pub async fn delete(&self, key: &str) -> Result<()> {
        match self {
            MediaStore::Local(s) => s.delete(key).await,
            MediaStore::S3(s) => s.delete(key).await,
        }
    }

    /// List every stored key under `prefix` (recursive), with sizes.
    /// The cleanup jobs and `delete_account_files` rely on this.
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<StoredObject>> {
        match self {
            MediaStore::Local(s) => s.list_prefix(prefix).await,
            MediaStore::S3(s) => s.list_prefix(prefix).await,
        }
    }
}

/// The local-filesystem backend.
#[derive(Clone)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    /// Absolute filesystem path for a storage key. Keys outside the
    /// served prefixes still resolve — the serving route's allow-list
    /// is what makes them unpublic, not the path.
    fn absolute_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.absolute_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.absolute_path(key);
        tokio::fs::read(&path).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound("file".into()),
            _ => Error::Io(e),
        })
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.absolute_path(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<StoredObject>> {
        let base = self.root.clone();
        let root = self.root.clone();
        let prefix = prefix.to_string();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            list_recursive(&root, &base.join(&prefix), &mut out)?;
            Ok(out)
        })
        .await
        .map_err(|e| Error::Internal(format!("media listing failed: {e}")))?
    }
}

/// Depth-first walk collecting `{key, size}` — `dir` is under `root`;
/// a missing prefix dir yields an empty list, not an error.
fn list_recursive(root: &Path, dir: &Path, out: &mut Vec<StoredObject>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in entries {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.is_dir() {
            list_recursive(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| Error::Internal(format!("media path outside root: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(StoredObject { key: rel, size });
        }
    }
    Ok(())
}

/// S3-compatible backend (`aws-sdk-s3`): endpoint, region, path-style
/// addressing and credentials come from `media.s3_*` config. The
/// client is `Arc`-backed internally — clones share the connector.
#[derive(Clone)]
pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Store {
    /// Build the client from `config.media.s3_*`. Credentials and the
    /// bucket are required up front — this backend runs without a
    /// default credential chain, so missing keys must panic at startup.
    fn from_config(config: &Config) -> Self {
        let media = &config.media;
        let bucket = media
            .s3_bucket
            .clone()
            .expect("media.s3_bucket is required when media.backend = \"s3\"");
        let (access, secret) = match (&media.s3_access_key, &media.s3_secret_key) {
            (Some(a), Some(s)) => (a.clone(), s.clone()),
            _ => panic!(
                "media.s3_access_key / media.s3_secret_key are required when media.backend = \"s3\""
            ),
        };
        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(
                media
                    .s3_region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
            ))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                access, secret, None, None, "yonder",
            ))
            .force_path_style(media.s3_force_path_style);
        if let Some(endpoint) = &media.s3_endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        S3Store {
            client: aws_sdk_s3::Client::from_conf(builder.build()),
            bucket,
        }
    }

    async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes.to_vec()))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("s3 put {key}: {e}")))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.as_service_error().is_some_and(|s| s.is_no_such_key()) {
                    Error::NotFound("media".into())
                } else {
                    Error::Internal(format!("s3 get {key}: {e}"))
                }
            })?;
        let bytes = out
            .body
            .collect()
            .await
            .map_err(|e| Error::Internal(format!("s3 get {key} body: {e}")))?;
        Ok(bytes.into_bytes().to_vec())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("s3 delete {key}: {e}")))?;
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<StoredObject>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let page = req
                .send()
                .await
                .map_err(|e| Error::Internal(format!("s3 list {prefix}: {e}")))?;
            for object in page.contents() {
                if let Some(key) = object.key() {
                    out.push(StoredObject {
                        key: key.to_string(),
                        size: object.size().unwrap_or(0) as u64,
                    });
                }
            }
            token = page.next_continuation_token().map(|t| t.to_string());
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

/// Remove every stored artifact owned by `account_id`: profile images,
/// status media, plus the private export/import trees (those never
/// pass the serving route's allow-list anyway).
pub async fn delete_account_files(store: &MediaStore, account_id: i64) -> Result<()> {
    for prefix in [
        "avatars",
        "headers",
        "exports",
        "imports",
        "media_attachments",
    ] {
        for object in store
            .list_prefix(&format!("{prefix}/{account_id}/"))
            .await?
        {
            store.delete(&object.key).await?;
        }
    }
    Ok(())
}

/// The serving router — merged in `http::build_router` for every
/// backend: on `local` it reads the filesystem, on `s3` it proxies the
/// bucket so one URL shape works for both.
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/media/{*key}", get(serve_media))
}

/// A storage key is servable only when it stays inside an allow-listed
/// prefix with the right shape and names a known extension.
///
/// Prefixes and their segment counts (below the prefix):
/// `avatars`, `headers`, `media_attachments`, `emoji` →
/// `{prefix}/{account}/{file}` (two segments); `previews` →
/// `{prefix}/{file}` (one).
fn valid_key(key: &str) -> bool {
    if key.starts_with('/') || key.ends_with('/') {
        return false;
    }
    let parts: Vec<&str> = key.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "..") {
        return false;
    }
    let Some(&prefix) = parts.first() else {
        return false;
    };
    let expected = match prefix {
        "avatars" | "headers" | "media_attachments" | "emoji" => 3,
        "previews" => 2,
        _ => return false,
    };
    parts.len() == expected && super::images::content_type_for_key(key).is_some()
}

async fn serve_media(
    State(state): State<Arc<AppState>>,
    AxumPath(key): AxumPath<String>,
) -> Result<impl IntoResponse> {
    if !valid_key(&key) {
        return Err(Error::NotFound("media".into()));
    }
    let bytes = state.media.get(&key).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Some(ct) = super::images::content_type_for_key(&key) {
        if let Ok(v) = HeaderValue::from_str(ct) {
            headers.insert(header::CONTENT_TYPE, v);
        }
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31556952, immutable"),
    );
    Ok((StatusCode::OK, headers, bytes))
}
