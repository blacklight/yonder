//! Configuration key discovery.
//!
//! Serializes [`Config::default()`] and walks the tree, emitting for every
//! leaf the pair of names an admin UI or `yonder config` invocation needs:
//! the dotted TOML path and the equivalent `YONDER_*` environment variable.
//! `config_overlays` rows use the same dotted-path format for their keys.

use serde_json::Value;

use super::{Config, ENV_PREFIX};

/// One discoverable configuration key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigKey {
    /// Dotted TOML path, e.g. `rate_limit.search.limit`.
    pub toml_path: String,
    /// Environment variable name, e.g. `YONDER_RATE_LIMIT__SEARCH__LIMIT`.
    pub env: String,
}

/// Every configurable leaf, sorted by TOML path.
pub fn entries() -> Vec<ConfigKey> {
    let value =
        serde_json::to_value(Config::default()).expect("Config::default() is always serializable");
    let mut out = Vec::new();
    walk(&value, "", &mut out);
    out.sort_by(|a, b| a.toml_path.cmp(&b.toml_path));
    out
}

/// Look up a single key by its dotted TOML path.
pub fn find(toml_path: &str) -> Option<ConfigKey> {
    entries().into_iter().find(|e| e.toml_path == toml_path)
}

/// The environment variable name for a dotted TOML path.
pub fn env_name(toml_path: &str) -> String {
    format!(
        "{}{}",
        ENV_PREFIX,
        toml_path.replace('.', "__").to_uppercase()
    )
}

fn walk(value: &Value, prefix: &str, out: &mut Vec<ConfigKey>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(child, &path, out);
            }
        }
        _ => out.push(ConfigKey {
            toml_path: prefix.to_string(),
            env: env_name(prefix),
        }),
    }
}
