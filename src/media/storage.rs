//! Local-backend storage: key ↔ public URL mapping and the
//! `GET /media/{*key}` serving route.
//!
//! Storage keys live under `media.local_path` and are the value stored
//! in `avatar_path`/`banner_path` — [`public_url`] renders the absolute
//! URL at read time, so `federation.base_url` or a future CDN change
//! doesn't invalidate stored data.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::http::AppState;
use crate::{Error, Result};

/// `/{base}/media/{key}` — the absolute URL clients and the actor
/// document (`icon_url`) see for a stored key.
pub fn public_url(base_url: &str, key: &str) -> String {
    format!("{}/media/{}", base_url.trim_end_matches('/'), key)
}

/// The serving router for `media.backend == "local"`. Merged in
/// `http::build_router`.
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/media/{*key}", get(serve_media))
}

/// Absolute filesystem path for a storage key (local backend only).
/// Keys outside the private prefixes still resolve — the serving
/// route's allow-list is what makes them unpublic, not the path.
pub fn absolute_path(config: &crate::config::Config, key: &str) -> std::path::PathBuf {
    std::path::Path::new(&config.media.local_path).join(key)
}

/// Write `bytes` to `key` on the local backend, creating parent dirs.
/// Used for private artifacts (archive exports, pending imports) that
/// must never be served by `GET /media/{*key}` — the route allow-list
/// keeps every non-`avatars`/`headers` prefix private.
pub async fn write_bytes(config: &crate::config::Config, key: &str, bytes: &[u8]) -> Result<()> {
    let path = absolute_path(config, key);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, bytes).await?;
    Ok(())
}

/// Read `key` from the local backend. `NotFound` maps to the crate's
/// `NotFound` error.
pub async fn read_bytes(config: &crate::config::Config, key: &str) -> Result<Vec<u8>> {
    let path = absolute_path(config, key);
    tokio::fs::read(&path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound("file".into()),
        _ => Error::Io(e),
    })
}

/// Remove a single stored key if it exists (idempotent).
pub async fn delete_key(config: &crate::config::Config, key: &str) -> Result<()> {
    let path = absolute_path(config, key);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Remove every stored artifact owned by `account_id` on the local
/// backend: profile images plus the private export/import trees. The
/// S3 backend is a no-op until epic 18 lands it.
pub async fn delete_account_files(config: &crate::config::Config, account_id: i64) -> Result<()> {
    if !matches!(config.media.backend, crate::config::MediaBackend::Local) {
        return Ok(());
    }
    let base = std::path::Path::new(&config.media.local_path);
    for prefix in [
        "avatars",
        "headers",
        "exports",
        "imports",
        "media_attachments",
    ] {
        let dir = base.join(format!("{prefix}/{account_id}"));
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

/// A storage key is servable only when it stays inside an allow-listed
/// prefix and names a known image extension.
fn valid_key(key: &str) -> bool {
    if key.starts_with('/') || key.ends_with('/') {
        return false;
    }
    let mut parts = key.split('/');
    let Some(prefix) = parts.next() else {
        return false;
    };
    if !matches!(prefix, "avatars" | "headers") {
        return false;
    }
    if parts.clone().any(|p| p.is_empty() || p == "..") {
        return false;
    }
    // Exactly one key segment below the prefix (`{dir}/{account}/{file}`).
    parts.clone().count() == 2 && super::images::content_type_for_key(key).is_some()
}

async fn serve_media(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse> {
    if !valid_key(&key) {
        return Err(Error::NotFound("media".into()));
    }
    let path = std::path::Path::new(&state.config.media.local_path).join(&key);
    let bytes = tokio::fs::read(&path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound("media".into()),
        _ => Error::Io(e),
    })?;
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
