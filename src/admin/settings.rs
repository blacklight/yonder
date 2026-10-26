//! Instance-settings surface for the admin panel (epic 22): describe
//! every configuration key — TOML path, env var, effective value and
//! its source — and apply or reset `config_overlays` rows.
//!
//! Policies encoded here:
//!
//! - [`READ_ONLY_SECTIONS`]: infrastructure sections whose consumers
//!   are process-level (bind address, pools, job worker). They are
//!   listed for the UI but cannot be overlaid — a runtime override
//!   could leave the instance unreachable.
//! - [`SENSITIVE_KEYS`]: secrets are write-only — descriptions report
//!   `set: true/false`, never the value.
//! - [`requires_restart`]: keys consumed by structures built once at
//!   startup (CORS layer, rate-limit buckets, `MediaStore`,
//!   `Dispatcher`, `ApCore`, the cron scheduler). The overlay lands in
//!   `state.config()` but the consumer keeps its startup value until
//!   the next boot — the UI warns via this flag.

use std::collections::HashSet;

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{
    config::{registry, Config},
    db,
    http::AppState,
    Error, Result,
};

/// Top-level config sections that cannot be overlaid.
const READ_ONLY_SECTIONS: &[&str] = &["server", "database", "redis", "jobs"];

/// Keys whose values never leave the process. Connection strings are
/// included because they carry database and cache passwords.
const SENSITIVE_KEYS: &[&str] = &[
    "database.url",
    "redis.url",
    "auth.secret_key",
    "email.smtp_password",
    "http.vapid_private_key",
    "content.google_translate_api_key",
    "content.deepl_api_key",
    "media.s3_access_key",
    "media.s3_secret_key",
];

/// Whether `key` exists in the registry and lives outside a read-only
/// section.
pub fn is_writable(key: &str) -> bool {
    let Some(entry) = registry::find(key) else {
        return false;
    };
    let section = entry.toml_path.split('.').next().unwrap_or_default();
    !READ_ONLY_SECTIONS.contains(&section)
}

/// Whether `key` holds a secret (write-only in the API).
pub fn is_sensitive(key: &str) -> bool {
    SENSITIVE_KEYS.contains(&key)
}

/// Whether an overlay on `key` only takes effect after restart —
/// either the whole section feeds startup construction, the key
/// drives a startup-built structure, or it names a cron schedule the
/// scheduler registers at boot.
pub fn requires_restart(key: &str) -> bool {
    const SECTIONS: &[&str] = &[
        "server",
        "database",
        "redis",
        "jobs",
        "rate_limit",
        "federation",
    ];
    const KEYS: &[&str] = &[
        "http.cors_enabled",
        "http.cors_origins",
        "admin.max_import_size",
        "media.backend",
        "media.local_path",
        "media.s3_endpoint",
        "media.s3_bucket",
        "media.s3_access_key",
        "media.s3_secret_key",
        "media.s3_region",
        "media.s3_force_path_style",
        "media.s3_public_base_url",
    ];
    let section = key.split('.').next().unwrap_or_default();
    SECTIONS.contains(&section) || KEYS.contains(&key) || key.ends_with("_schedule")
}

/// Walk `value` down a dotted path; missing segments yield `Null`.
fn leaf(value: &Value, path: &str) -> Value {
    let mut node = value;
    for part in path.split('.') {
        match node.get(part) {
            Some(child) => node = child,
            None => return Value::Null,
        }
    }
    node.clone()
}

/// Keys currently overridden by `config_overlays` rows.
async fn overlay_keys(pool: &PgPool) -> Result<HashSet<String>> {
    Ok(db::config_overlays::fetch(pool)
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect())
}

/// Build the description JSON for one registry key. `overlays`,
/// `effective` and `defaults` are passed in so `describe_all` can
/// compute them once.
fn describe_key(
    overlays: &HashSet<String>,
    effective: &Value,
    defaults: &Value,
    key: &registry::ConfigKey,
) -> Value {
    let sensitive = is_sensitive(&key.toml_path);
    let overlaid = overlays.contains(&key.toml_path);
    let env_set = std::env::var_os(&key.env).is_some();
    let source = if overlaid {
        "overlay"
    } else if env_set {
        "env"
    } else {
        "default"
    };
    let mut entry = json!({
        "toml_path": key.toml_path,
        "env": key.env,
        "source": source,
        "requires_restart": requires_restart(&key.toml_path),
        "sensitive": sensitive,
        "writable": is_writable(&key.toml_path),
    });
    if sensitive {
        entry["default"] = Value::Null;
        entry["value"] = Value::Null;
        entry["set"] = Value::Bool(overlaid || env_set);
    } else {
        entry["default"] = leaf(defaults, &key.toml_path);
        entry["value"] = leaf(effective, &key.toml_path);
    }
    entry
}

/// Describe one key by dotted TOML path.
pub async fn describe(state: &AppState, key: &str) -> Result<Value> {
    let Some(entry) = registry::find(key) else {
        return Err(Error::NotFound(format!("unknown config key {key:?}")));
    };
    let overlays = overlay_keys(&state.db).await?;
    let effective = serde_json::to_value(&*state.config()).expect("Config is always serializable");
    let defaults = serde_json::to_value(Config::default()).expect("Config is always serializable");
    Ok(describe_key(&overlays, &effective, &defaults, &entry))
}

/// Describe every registry key, optionally narrowed to one top-level
/// section (`?section=rate_limit`).
pub async fn describe_all(state: &AppState, section: Option<&str>) -> Result<Vec<Value>> {
    let overlays = overlay_keys(&state.db).await?;
    let effective = serde_json::to_value(&*state.config()).expect("Config is always serializable");
    let defaults = serde_json::to_value(Config::default()).expect("Config is always serializable");
    Ok(registry::entries()
        .iter()
        .filter(|e| {
            section.is_none_or(|s| e.toml_path == s || e.toml_path.starts_with(&format!("{s}.")))
        })
        .map(|e| describe_key(&overlays, &effective, &defaults, e))
        .collect())
}

/// Guard shared by set and reset: the key must exist and be writable.
fn guard(key: &str) -> Result<()> {
    if registry::find(key).is_none() {
        return Err(Error::NotFound(format!("unknown config key {key:?}")));
    }
    if !is_writable(key) {
        return Err(Error::Forbidden("config key is read-only"));
    }
    Ok(())
}

/// Validate `value` for `key`, store it as an overlay and reload the
/// effective config. Validation dry-runs `Config::with_overlays` with
/// the candidate row merged over the current ones — type and range
/// errors surface as 422 before anything is written.
///
/// Returns the key's fresh description.
pub async fn validate_and_apply(state: &AppState, key: &str, value: Value) -> Result<Value> {
    guard(key)?;
    let mut overlays = db::config_overlays::fetch(&state.db).await?;
    overlays.retain(|(k, _)| k != key);
    overlays.push((key.to_string(), value.clone()));
    // Dry-run against the startup-resolved config — the same merge
    // `reload_config` performs.
    state.config_with_overlays(&overlays).map_err(|e| {
        // figment's message can echo the submitted value — keep it out
        // of the response for secrets.
        let detail = if is_sensitive(key) {
            "rejected".to_string()
        } else {
            e.to_string()
        };
        Error::Validation(format!("invalid value for {key}: {detail}"))
    })?;
    db::config_overlays::upsert(&state.db, key, &value).await?;
    state.reload_config().await?;
    describe(state, key).await
}

/// Remove `key`'s overlay (idempotent) and reload the effective
/// config, which falls back to env → TOML → default.
pub async fn reset(state: &AppState, key: &str) -> Result<Value> {
    guard(key)?;
    db::config_overlays::delete(&state.db, key).await?;
    state.reload_config().await?;
    describe(state, key).await
}
