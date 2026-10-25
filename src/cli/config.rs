//! `yonder config` and `yonder instance` — database overlay
//! management over `config_overlays`, with the same
//! writability/sensitivity/restart policy metadata the admin API
//! uses (`admin::settings`). Every write validates the resulting
//! candidate config before it lands, and commits the overlay row
//! and its audit row in one transaction.

use std::collections::HashSet;
use std::path::Path;

use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::{
    admin::settings,
    config::{registry, Config},
    db,
    moderation::audit,
    Error, Result,
};

use super::{
    context,
    output::{self, Output},
};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// List every configuration key as `toml.path` ↔ `YONDER_ENV_NAME`.
    List,
    /// Print the effective value of a configuration key (dotted TOML
    /// path), resolved from database overlays → env → config.toml →
    /// defaults. Sensitive keys are redacted unless --reveal.
    Get {
        /// Configuration key (dotted TOML path), e.g. instance.name.
        key: String,
        /// Show the stored value instead of `<redacted>`.
        #[arg(long)]
        reveal: bool,
    },
    /// Store a database overlay for a key. `value` is parsed as JSON
    /// when it parses (`true`, `42`, `["en"]`, `"x"`), else used as a
    /// string. The resulting configuration is validated before the
    /// overlay is written.
    Set {
        /// Configuration key (dotted TOML path).
        key: String,
        /// New value — JSON literal, or a bare string.
        value: String,
    },
    /// Remove a key's database overlay; falls back to env → TOML →
    /// default. Removing a key that has no overlay is a no-op.
    Unset {
        /// Configuration key (dotted TOML path).
        key: String,
    },
    /// Print the effective configuration; sensitive keys are
    /// redacted unless --reveal.
    Show {
        /// Include a per-key provenance map: `db` (overlay), `env`,
        /// `toml`, or `default`.
        #[arg(long)]
        source: bool,
        /// Show sensitive values instead of `<redacted>`.
        #[arg(long)]
        reveal: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum InstanceCommand {
    /// Set the instance's public domain — a bare DNS hostname such as
    /// `example.com` (no scheme, path, port or credentials).
    SetDomain {
        /// The public domain name.
        domain: String,
    },
    /// Set the instance's display name.
    SetName {
        /// The display name (must not be blank).
        name: String,
    },
    /// Show the effective instance settings.
    Show,
}

pub async fn run(
    path: Option<&Path>,
    command: ConfigCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        ConfigCommand::List => list(out),
        ConfigCommand::Get { key, reveal } => get(path, &key, reveal, out).await,
        ConfigCommand::Set { key, value } => {
            let value = parse_value(&value);
            set_key(path, &key, value, out, actor).await
        }
        ConfigCommand::Unset { key } => unset(path, &key, out, actor).await,
        ConfigCommand::Show { source, reveal } => show(path, source, reveal, out).await,
    }
}

pub async fn run_instance(
    path: Option<&Path>,
    command: InstanceCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        InstanceCommand::SetDomain { domain } => {
            let domain = validate_instance_domain(&domain)?;
            let (_, pool, overlays) = context::load_overlay_editor(path).await?;
            let current = Config::with_overlays(path, &overlays)
                .map(|c| c.instance.domain)
                .unwrap_or_default();
            if current != domain {
                out.warn(&format!(
                    "changing the instance domain rewrites federation URLs and \
                     actor ids ({current} → {domain})"
                ));
            }
            let code = set_key(path, "instance.domain", Value::String(domain), out, actor).await?;
            drop(pool);
            Ok(code)
        }
        InstanceCommand::SetName { name } => {
            if name.trim().is_empty() {
                return Err(Error::Validation(
                    "instance name must not be blank".to_string(),
                ));
            }
            set_key(path, "instance.name", Value::String(name), out, actor).await
        }
        InstanceCommand::Show => {
            let ctx = context::load_db(path).await?;
            let mut value =
                serde_json::to_value(&ctx.config).map_err(|e| Error::Internal(e.to_string()))?;
            output::redact_config(&mut value);
            let instance = value
                .get_mut("instance")
                .map(std::mem::take)
                .unwrap_or(Value::Null);
            if out.json_mode() {
                out.emit_json(&instance)?;
            } else {
                out.emit_human(
                    &serde_json::to_string_pretty(&instance)
                        .map_err(|e| Error::Internal(e.to_string()))?,
                )?;
            }
            Ok(0)
        }
    }
}

/// `value` is JSON when it parses, a bare string otherwise — so
/// `true`, `42`, `["en"]` and `"x"` are typed, while `example.com`
/// stays a string.
fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn list(out: &Output) -> Result<u8> {
    let entries = registry::entries();
    if out.json_mode() {
        let rows: Vec<Value> = entries
            .iter()
            .map(|e| json!({"toml_path": e.toml_path, "env": e.env}))
            .collect();
        out.emit_json(&Value::Array(rows))?;
    } else {
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|e| vec![e.toml_path.clone(), e.env.clone()])
            .collect();
        out.table(&["toml_path", "env"], &rows)?;
    }
    Ok(0)
}

async fn get(path: Option<&Path>, key: &str, reveal: bool, out: &Output) -> Result<u8> {
    if registry::find(key).is_none() {
        return Err(Error::NotFound(format!("config key {key:?}")));
    }
    let ctx = context::load_db(path).await?;
    let value = serde_json::to_value(&ctx.config).map_err(|e| Error::Internal(e.to_string()))?;
    let leaf = key
        .split('.')
        .try_fold(&value, |node, part| node.get(part))
        .expect("key exists in the registry")
        .clone();
    let leaf = if !reveal && settings::is_sensitive(key) {
        Value::String("<redacted>".to_string())
    } else {
        leaf
    };
    if out.json_mode() {
        out.emit_json(&leaf)?;
    } else {
        match &leaf {
            Value::String(s) => out.emit_human(s)?,
            other => out.emit_human(
                &serde_json::to_string_pretty(other).map_err(|e| Error::Internal(e.to_string()))?,
            )?,
        }
    }
    Ok(0)
}

/// Render a candidate-resolution error without leaking secrets.
/// `sensitive` refers to the key being edited — its submitted value
/// is never echoed. Even when it is not sensitive, figment errors can
/// embed *other* layers' values (an existing sensitive overlay or a
/// secret from TOML/env), so the message is scrubbed of every
/// sensitive value present in the candidate inputs.
fn candidate_error(key: &str, error: &Error, sensitive: bool, secrets: &[String]) -> Error {
    let detail = if sensitive {
        "rejected".to_string()
    } else {
        let mut message = error.to_string();
        for secret in secrets {
            message = message.replace(secret.as_str(), "<redacted>");
        }
        message
    };
    Error::Validation(format!("invalid value for {key}: {detail}"))
}

/// Every sensitive value currently feeding the candidate config —
/// sensitive overlay rows plus sensitive leaves of the base config —
/// as plain strings, longest first so replacements can't shadow each
/// other. Values shorter than 4 chars are skipped to avoid mangling
/// unrelated text.
fn sensitive_value_strings(base: &Config, overlays: &[(String, Value)]) -> Vec<String> {
    let mut values = Vec::new();
    fn push_leaf(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Number(_) | Value::Bool(_) => out.push(value.to_string()),
            Value::Array(items) => {
                for item in items {
                    push_leaf(item, out);
                }
            }
            _ => {}
        }
    }
    for (key, value) in overlays {
        if settings::is_sensitive(key) {
            push_leaf(value, &mut values);
        }
    }
    if let Ok(base) = serde_json::to_value(base) {
        fn walk(node: &Value, path: &str, out: &mut Vec<String>) {
            match node {
                Value::Object(map) => {
                    for (key, child) in map {
                        let child_path = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        walk(child, &child_path, out);
                    }
                }
                Value::Array(items) if settings::is_sensitive(path) => {
                    for item in items {
                        if let Value::String(s) = item {
                            out.push(s.clone());
                        }
                    }
                }
                Value::String(s) if settings::is_sensitive(path) => out.push(s.clone()),
                Value::Number(_) | Value::Bool(_) if settings::is_sensitive(path) => {
                    out.push(node.to_string());
                }
                _ => {}
            }
        }
        walk(&base, "", &mut values);
    }
    values.retain(|v| v.len() >= 4);
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    values.dedup();
    values
}

/// The shared overlay writer behind `config set`, `instance
/// set-domain` and `instance set-name`: registry + writability
/// checks, candidate validation (nothing is persisted on failure),
/// then one transaction committing the overlay row and its
/// `config.set` audit row. Sensitive values are never echoed — not
/// in output, errors, or audit details.
async fn set_key(
    path: Option<&Path>,
    key: &str,
    value: Value,
    out: &Output,
    actor_name: Option<&str>,
) -> Result<u8> {
    if registry::find(key).is_none() {
        return Err(Error::NotFound(format!("config key {key:?}")));
    }
    if !settings::is_writable(key) {
        return Err(Error::Validation(format!(
            "{key} is read-only (sections server/database/redis/jobs cannot be overlaid)"
        )));
    }
    let sensitive = settings::is_sensitive(key);

    // The base config must itself be valid; `load_overlay_editor`
    // skips effective-config validation so a bad overlay can be fixed.
    let (base, pool, mut overlays) = context::load_overlay_editor(path).await?;
    if !db::config_overlays::table_exists(&pool).await? {
        return Err(Error::Validation(
            "the config_overlays table is missing — run `yonder migrate` first".to_string(),
        ));
    }
    let secrets = sensitive_value_strings(&base, &overlays);
    overlays.retain(|(k, _)| k != key);
    overlays.push((key.to_string(), value.clone()));
    let candidate = Config::with_overlays(path, &overlays)
        .map_err(|e| candidate_error(key, &e, sensitive, &secrets))?;
    candidate
        .validate()
        .map_err(|e| candidate_error(key, &e, sensitive, &secrets))?;

    let actor_id = context::resolve_audit_actor(&pool, actor_name).await?;
    let mut tx = pool.begin().await?;
    db::config_overlays::upsert_tx(&mut tx, key, &value).await?;
    audit::record(
        &mut *tx,
        actor_id,
        "config.set",
        "config",
        None,
        json!({
            "key": key,
            "value": if sensitive { Value::String("<redacted>".into()) } else { value },
            "sensitive": sensitive,
        }),
    )
    .await?;
    tx.commit().await?;

    if sensitive {
        out.warn("note: this value is stored in the database in plaintext");
    }
    if settings::requires_restart(key) {
        out.warn("note: this key only takes effect on restart");
    }
    if out.json_mode() {
        out.emit_json(&json!({"key": key, "updated": true}))?;
    } else {
        out.emit_human(&format!("set {key}"))?;
    }
    Ok(0)
}

async fn unset(
    path: Option<&Path>,
    key: &str,
    out: &Output,
    actor_name: Option<&str>,
) -> Result<u8> {
    if registry::find(key).is_none() {
        return Err(Error::NotFound(format!("config key {key:?}")));
    }
    if !settings::is_writable(key) {
        return Err(Error::Validation(format!(
            "{key} is read-only (sections server/database/redis/jobs cannot be overlaid)"
        )));
    }
    let (_base, pool, overlays) = context::load_overlay_editor(path).await?;
    if !db::config_overlays::table_exists(&pool).await? {
        return Err(Error::Validation(
            "the config_overlays table is missing — run `yonder migrate` first".to_string(),
        ));
    }
    if !overlays.iter().any(|(k, _)| k == key) {
        out.warn(&format!("no overlay set for {key}"));
        if out.json_mode() {
            out.emit_json(&json!({"key": key, "removed": false}))?;
        }
        return Ok(0);
    }
    // The resulting configuration must still resolve and validate
    // before the row is dropped.
    let secrets = sensitive_value_strings(&_base, &overlays);
    let remaining: Vec<(String, Value)> = overlays.into_iter().filter(|(k, _)| k != key).collect();
    let scrub = |e: &Error| -> Error {
        let mut message = e.to_string();
        for secret in &secrets {
            message = message.replace(secret.as_str(), "<redacted>");
        }
        Error::Validation(format!("cannot remove overlay for {key}: {message}"))
    };
    let candidate = Config::with_overlays(path, &remaining).map_err(|e| scrub(&e))?;
    candidate.validate().map_err(|e| scrub(&e))?;

    let actor_id = context::resolve_audit_actor(&pool, actor_name).await?;
    let mut tx = pool.begin().await?;
    db::config_overlays::delete_tx(&mut tx, key).await?;
    audit::record(
        &mut *tx,
        actor_id,
        "config.unset",
        "config",
        None,
        json!({"key": key}),
    )
    .await?;
    tx.commit().await?;

    if out.json_mode() {
        out.emit_json(&json!({"key": key, "removed": true}))?;
    } else {
        out.emit_human(&format!("unset {key}"))?;
    }
    Ok(0)
}

async fn show(path: Option<&Path>, source: bool, reveal: bool, out: &Output) -> Result<u8> {
    let ctx = context::load_db(path).await?;
    let mut value =
        serde_json::to_value(&ctx.config).map_err(|e| Error::Internal(e.to_string()))?;
    if !reveal {
        output::redact_config(&mut value);
    }
    if !source {
        return emit_config(out, &value);
    }
    let overlay_keys: HashSet<String> = db::config_overlays::fetch(&ctx.pool)
        .await?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let toml_keys = toml_dotted_keys(path)?;
    let mut sources = Map::new();
    for entry in registry::entries() {
        let src = if overlay_keys.contains(&entry.toml_path) {
            "db"
        } else if std::env::var_os(&entry.env).is_some() {
            "env"
        } else if toml_keys.contains(&entry.toml_path) {
            "toml"
        } else {
            "default"
        };
        sources.insert(entry.toml_path, Value::String(src.to_string()));
    }
    emit_config(
        out,
        &json!({"config": value, "sources": Value::Object(sources)}),
    )
}

fn emit_config(out: &Output, value: &Value) -> Result<u8> {
    if out.json_mode() {
        out.emit_json(value)?;
    } else {
        out.emit_human(
            &serde_json::to_string_pretty(value).map_err(|e| Error::Internal(e.to_string()))?,
        )?;
    }
    Ok(0)
}

/// The dotted leaf keys explicitly present in the config TOML file —
/// the `toml` provenance for `config show --source`. Figment doesn't
/// expose per-key origins, so the file is read through its own
/// provider. A missing file yields an empty set.
fn toml_dotted_keys(path: Option<&Path>) -> Result<HashSet<String>> {
    use figment::{providers::Format, Provider};
    let file = Config::config_path(path);
    let mut out = HashSet::new();
    if !file.is_file() {
        return Ok(out);
    }
    let data = figment::providers::Toml::file(&file)
        .data()
        .map_err(|e| Error::Config(Box::new(e)))?;
    for dict in data.values() {
        flatten_toml(dict, "", &mut out);
    }
    Ok(out)
}

fn flatten_toml(dict: &figment::value::Dict, prefix: &str, out: &mut HashSet<String>) {
    for (key, value) in dict {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if let figment::value::Value::Dict(_, inner) = value {
            flatten_toml(inner, &path, out);
        } else {
            out.insert(path);
        }
    }
}

/// A bare DNS hostname for `instance.domain`: letters, digits,
/// hyphens and dots only, well-formed labels, no scheme, path, port,
/// credentials or whitespace. Shared by `instance set-domain` and
/// `config set instance.domain`.
pub fn validate_instance_domain(raw: &str) -> Result<String> {
    let domain = raw.trim().to_lowercase();
    let ok = !domain.is_empty()
        && domain.len() <= 253
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && domain
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if ok {
        Ok(domain)
    } else {
        Err(Error::Validation(format!(
            "invalid domain {raw:?} — expected a bare DNS hostname like example.com"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_parses_as_json_then_string() {
        assert_eq!(parse_value("42"), json!(42));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value("[\"a\"]"), json!(["a"]));
        assert_eq!(parse_value("\"x\""), json!("x"));
        assert_eq!(parse_value("example.com"), json!("example.com"));
        assert_eq!(
            parse_value("unquoted with spaces"),
            json!("unquoted with spaces")
        );
    }

    #[test]
    fn domain_validation() {
        for ok in ["example.com", "sub.example.com", "localhost", "EXAMPLE.com"] {
            assert!(validate_instance_domain(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "https://example.com",
            "example.com:8443",
            "example.com/path",
            "user@example.com",
            "example .com",
            "-example.com",
            "example..com",
        ] {
            assert!(validate_instance_domain(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_instance_domain("Example.COM").unwrap(),
            "example.com"
        );
    }
}
