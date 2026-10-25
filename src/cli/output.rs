//! Output plumbing for the `yonder` CLI.
//!
//! The contract is simple and load-bearing: **stdout carries exactly
//! one JSON value in `--json` mode** (or the command's human output
//! otherwise), and every diagnostic — warnings, errors, secrets the
//! user must see once — goes to **stderr**. Result-bearing commands
//! pick exactly one of [`Output::emit_json`] / [`Output::emit_human`]
//! per invocation; there is deliberately no combined "fallback"
//! method so a command can never silently print the wrong shape.

use std::io::Write;

use serde_json::Value;

use crate::{admin::settings, Error, Result};

/// Rendering mode for one invocation, built from the global
/// `--json` flag.
pub struct Output {
    json: bool,
}

impl Output {
    pub fn new(json: bool) -> Self {
        Output { json }
    }

    /// Whether `--json` was requested.
    pub fn json_mode(&self) -> bool {
        self.json
    }

    /// Print one serialized JSON value plus a newline on stdout.
    /// Call only in `--json` mode.
    pub fn emit_json(&self, value: &Value) -> Result<()> {
        let body =
            serde_json::to_string_pretty(value).map_err(|e| Error::Internal(e.to_string()))?;
        writeln!(std::io::stdout(), "{body}")?;
        Ok(())
    }

    /// Print a human-readable line (or table block) on stdout.
    /// Call only in non-`--json` mode.
    pub fn emit_human(&self, text: &str) -> Result<()> {
        writeln!(std::io::stdout(), "{text}")?;
        Ok(())
    }

    /// A diagnostic on stderr — never stdout, so `--json` output
    /// stays pipe-safe.
    pub fn warn(&self, message: &str) {
        eprintln!("{message}");
    }

    /// Print an aligned table on stdout (human mode). Column widths
    /// come from the widest cell in each column.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> Result<()> {
        let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }
        let fmt = |cells: &[String]| -> String {
            cells
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let w = widths.get(i).copied().unwrap_or(0);
                    if i + 1 == cells.len() {
                        c.clone()
                    } else {
                        format!("{c:<w$}")
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        };
        let header: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
        self.emit_human(&fmt(&header))?;
        for row in rows {
            self.emit_human(&fmt(row))?;
        }
        Ok(())
    }
}

/// Replace every sensitive leaf of a serialized [`crate::config::Config`]
/// tree with `"<redacted>"`. Traversal tracks the dotted path so the
/// `admin::settings::is_sensitive` list applies at any depth.
pub fn redact_config(value: &mut Value) {
    fn walk(node: &mut Value, path: &str) {
        match node {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, &child_path);
                }
            }
            _ => {
                if settings::is_sensitive(path) {
                    *node = Value::String("<redacted>".to_string());
                }
            }
        }
    }
    walk(value, "");
}

/// Strip `user:pass@` user-info from any URL embedded in a message so
/// a credential-bearing relay/git URL can never be echoed back.
fn redact_url_userinfo(message: &str) -> String {
    let mut out = message.to_string();
    let mut search_from = 0;
    while let Some(pos) = out[search_from..].find("://") {
        let start = search_from + pos + 3;
        let rest = &out[start..];
        let end = rest
            .find(|c: char| ['/', ' ', '"', '\''].contains(&c))
            .unwrap_or(rest.len());
        let authority = &rest[..end];
        if let Some(at) = authority.rfind('@') {
            out.replace_range(start..start + at + 1, "");
            // Rescan from the stripped position.
            search_from = start;
        } else {
            search_from = start + end;
        }
    }
    out
}

/// Render a command error: `--json` writes
/// `{"error": "...", "kind": "..."}` on stderr, human mode writes
/// `error: ...`. The message is the client-facing
/// [`Error::public_message`] — infrastructure variants stay generic
/// so internals (and any config value a figment error might echo)
/// never leak, and URL user-info is scrubbed either way.
pub fn render_error(error: &Error, json: bool) {
    let message = redact_url_userinfo(&error.public_message());
    if json {
        let body = serde_json::json!({
            "error": message,
            "kind": error.error_kind(),
        });
        eprintln!("{body}");
    } else {
        eprintln!("error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_stripped() {
        assert_eq!(
            redact_url_userinfo("invalid relay URL \"https://u:p@relay.example/inbox\""),
            "invalid relay URL \"https://relay.example/inbox\""
        );
        assert_eq!(
            redact_url_userinfo("git clone of https://token@example.com/repo failed"),
            "git clone of https://example.com/repo failed"
        );
        assert_eq!(redact_url_userinfo("plain message"), "plain message");
    }

    #[test]
    fn redaction_covers_sensitive_leaves() {
        let mut v = serde_json::json!({
            "email": {"smtp_password": "hunter2", "smtp_host": "mail.example"},
            "media": {"s3_secret_key": "abc", "s3_bucket": "b"},
            "instance": {"name": "yonder"},
        });
        redact_config(&mut v);
        assert_eq!(v["email"]["smtp_password"], "<redacted>");
        assert_eq!(v["media"]["s3_secret_key"], "<redacted>");
        assert_eq!(v["email"]["smtp_host"], "mail.example");
        assert_eq!(v["instance"]["name"], "yonder");
    }
}
