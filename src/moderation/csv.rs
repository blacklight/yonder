//! Mastodon-compatible CSV serialization for instance domain blocks.
//!
//! Mastodon v4.1+ exports this exact header:
//!
//! ```csv
//! #domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate
//! ```
//!
//! Its importer also accepts a headerless one-domain-per-line file
//! (the shape most Pleroma/Akkoma blocklists circulate in). Parsing is
//! deliberately a little more permissive on input — unprefixed
//! `domain`/`severity` headers, common severity aliases and an
//! optional `private_comment` column — while export stays byte-shaped
//! for Mastodon's own importer.

use std::collections::{HashMap, HashSet};

use csv::{ReaderBuilder, StringRecord, Trim, WriterBuilder};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{Error, Result};

use super::{
    audit,
    blocks::{self, BlockRow, Severity},
    domain::normalize_domain,
};

/// Mastodon's `Admin::Import::ROWS_PROCESSING_LIMIT`.
pub const ROWS_PROCESSING_LIMIT: usize = 20_000;

const EXPORT_HEADERS: [&str; 6] = [
    "#domain",
    "#severity",
    "#reject_media",
    "#reject_reports",
    "#public_comment",
    "#obfuscate",
];

/// One validated import row. `domain` is already canonicalized and
/// every optional field has received its Mastodon default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    /// 1-based physical CSV line where the record starts.
    pub line: u64,
    pub domain: String,
    pub severity: Severity,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub obfuscate: bool,
    pub private_comment: String,
    pub public_comment: String,
}

/// A row skipped because one of its fields could not be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIssue {
    pub line: u64,
    pub domain: String,
    pub error: String,
}

/// Valid rows plus row-level validation issues.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportParse {
    pub rows: Vec<ImportRow>,
    pub issues: Vec<ImportIssue>,
    /// Data rows seen before row-level validation.
    pub total_rows: usize,
}

fn csv_error(e: impl std::fmt::Display) -> Error {
    Error::Validation(format!("invalid CSV file: {e}"))
}

fn normalize_header(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('\u{feff}')
        .trim_start_matches('#')
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
}

fn field_positions(headers: &StringRecord) -> HashMap<String, usize> {
    let mut positions = HashMap::new();
    for (index, header) in headers.iter().enumerate() {
        positions.entry(normalize_header(header)).or_insert(index);
    }
    positions
}

fn field<'a>(
    record: &'a StringRecord,
    positions: &HashMap<String, usize>,
    name: &str,
) -> Option<&'a str> {
    positions
        .get(name)
        .and_then(|index| record.get(*index))
        .map(str::trim)
}

/// Parse the boolean vocabulary Rails' `ActiveModel::Type::Boolean`
/// accepts (`true/1/t/yes/on`, and their false counterparts). Blank
/// means the column was present but empty and is false, matching the
/// boolean cast used by Mastodon's importer.
pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        _ => None,
    }
}

/// CSV severities use Mastodon's canonical spellings; `limit` is
/// already accepted by [`Severity::parse`]. The `reject`/`suspended`
/// aliases cover blocklists exported in Akkoma/Pleroma MRF vocabulary
/// (`reject` is their full-defederation action).
fn parse_csv_severity(value: &str) -> Option<Severity> {
    match value.trim().to_ascii_lowercase().as_str() {
        "limited" | "silenced" => Some(Severity::Silence),
        "suspended" | "reject" | "rejected" | "block" | "blocked" => Some(Severity::Suspend),
        "none" => Some(Severity::Noop),
        canonical => Severity::parse(canonical),
    }
}

fn validate_row(
    record: &StringRecord,
    positions: Option<&HashMap<String, usize>>,
    line: u64,
) -> std::result::Result<ImportRow, ImportIssue> {
    let get = |name: &str| -> Option<&str> {
        if name == "domain" && positions.is_none() {
            record.get(0).map(str::trim)
        } else {
            positions.and_then(|p| field(record, p, name))
        }
    };
    let raw_domain = get("domain").unwrap_or_default();
    let domain = normalize_domain(raw_domain);
    let issue = |error: String| ImportIssue {
        line,
        domain: raw_domain.to_string(),
        error,
    };
    if domain.is_empty() {
        return Err(issue("invalid domain".into()));
    }

    let severity_value = get("severity");
    let severity = match severity_value {
        // Mastodon defaults a missing severity *column* to suspend; a
        // present-but-blank cell is invalid instead.
        None => Severity::Suspend,
        Some(value) => {
            let Some(parsed) = parse_csv_severity(value) else {
                return Err(issue(format!("invalid severity {value:?}")));
            };
            parsed
        }
    };
    let mut flags = HashMap::new();
    for name in ["reject_media", "reject_reports", "obfuscate"] {
        let value = get(name).unwrap_or_default();
        let Some(parsed) = parse_bool(value) else {
            return Err(issue(format!("invalid boolean for {name}: {value:?}")));
        };
        flags.insert(name, parsed);
    }

    let private_comment = get("private_comment").unwrap_or_default().to_string();
    let public_comment = get("public_comment").unwrap_or_default().to_string();
    if private_comment.chars().count() > 2000 || public_comment.chars().count() > 2000 {
        return Err(issue("comments are limited to 2000 characters".into()));
    }

    Ok(ImportRow {
        line,
        domain,
        severity,
        reject_media: flags["reject_media"],
        reject_reports: flags["reject_reports"],
        obfuscate: flags["obfuscate"],
        private_comment,
        public_comment,
    })
}

fn push_record(
    record: &StringRecord,
    positions: Option<&HashMap<String, usize>>,
    line: u64,
    out: &mut ImportParse,
) -> Result<()> {
    out.total_rows += 1;
    if out.total_rows > ROWS_PROCESSING_LIMIT {
        return Err(Error::Validation(format!(
            "CSV files are limited to {ROWS_PROCESSING_LIMIT} rows"
        )));
    }
    match validate_row(record, positions, line) {
        Ok(row) => out.rows.push(row),
        Err(issue) => out.issues.push(issue),
    }
    Ok(())
}

/// Parse a Mastodon/Pleroma/Akkoma-compatible CSV upload. Input with a
/// recognized `domain` first header is read as a headed CSV; otherwise
/// every line — including the first — is treated as a bare-domain row,
/// matching Mastodon's fallback for headerless blocklists.
pub fn parse_import(bytes: &[u8]) -> Result<ImportParse> {
    // Excel-originated CSVs often carry a UTF-8 BOM; it is metadata,
    // not part of the first domain/header cell.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .trim(Trim::All)
        .from_reader(bytes);
    let headers = reader.headers().map_err(csv_error)?.clone();
    if headers.is_empty() {
        return Ok(ImportParse::default());
    }
    let positions = field_positions(&headers);
    let headed = positions.get("domain") == Some(&0);
    let mut out = ImportParse::default();

    if !headed {
        let line = headers.position().map(|p| p.line()).unwrap_or(1);
        push_record(&headers, None, line, &mut out)?;
    }
    for record in reader.records() {
        let record = record.map_err(csv_error)?;
        let line = record.position().map(|p| p.line()).unwrap_or(0);
        push_record(&record, headed.then_some(&positions), line, &mut out)?;
    }
    Ok(out)
}

/// Serialize rows with Mastodon v4.1's exact export header and column
/// order. Private comments are deliberately not exported there (they
/// are staff-internal), so they are absent here too.
pub fn export(rows: &[BlockRow]) -> Result<Vec<u8>> {
    let mut writer = WriterBuilder::new().from_writer(vec![]);
    writer.write_record(EXPORT_HEADERS).map_err(csv_error)?;
    for row in rows {
        writer
            .write_record([
                row.domain.as_str(),
                row.severity.as_str(),
                &row.reject_media.to_string(),
                &row.reject_reports.to_string(),
                row.public_comment.as_str(),
                &row.obfuscate.to_string(),
            ])
            .map_err(csv_error)?;
    }
    writer.into_inner().map_err(csv_error)
}

/// Aggregate result of applying one parsed import file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub created: usize,
    pub updated: usize,
    pub skipped_existing: usize,
    pub skipped_invalid: usize,
    pub duplicates: usize,
    pub total_rows: usize,
    pub errors: Vec<ImportIssue>,
}

impl ImportReport {
    pub fn mutated(&self) -> bool {
        self.created + self.updated > 0
    }

    /// API/CLI machine-readable projection. Row errors are capped so
    /// a hostile or accidentally huge file cannot turn the response
    /// into a second large upload.
    pub fn json(&self) -> Value {
        json!({
            "created": self.created,
            "updated": self.updated,
            "skipped_existing": self.skipped_existing,
            "skipped_invalid": self.skipped_invalid,
            "duplicates": self.duplicates,
            "total_rows": self.total_rows,
            "errors": self.errors.iter().take(25).map(|issue| json!({
                "line": issue.line,
                "domain": issue.domain,
                "error": issue.error,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Apply a parsed import atomically, using the same counters and
/// audit shape for the HTTP endpoint and the CLI. Existing domains
/// are skipped unless `overwrite` is set; duplicate rows inside the
/// file are collapsed before counting so the last row wins without
/// inflating `created`/`updated`.
pub async fn apply_import(
    db: &PgPool,
    instance_domain: &str,
    parsed: ImportParse,
    overwrite: bool,
    filename: Option<&str>,
    actor_id: i64,
) -> Result<ImportReport> {
    let own_domain = normalize_domain(instance_domain);
    let mut issues = parsed.issues;
    let mut rows = Vec::new();
    for row in parsed.rows {
        if row.domain == own_domain {
            issues.push(ImportIssue {
                line: row.line,
                domain: row.domain,
                error: "cannot block your own instance".into(),
            });
        } else {
            rows.push(row);
        }
    }

    // Applying rows in file order would make the last duplicate win;
    // dedupe first so the counters reflect the same final state.
    let mut deduped: Vec<ImportRow> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut duplicates = 0usize;
    for row in rows {
        if let Some(index) = positions.get(&row.domain).copied() {
            deduped[index] = row;
            duplicates += 1;
        } else {
            positions.insert(row.domain.clone(), deduped.len());
            deduped.push(row);
        }
    }

    let domains: Vec<String> = deduped.iter().map(|row| row.domain.clone()).collect();
    let existing: HashSet<String> = if domains.is_empty() {
        HashSet::new()
    } else {
        sqlx::query_scalar::<_, String>("SELECT domain FROM domain_blocks WHERE domain = ANY($1)")
            .bind(&domains)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect()
    };

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut skipped_existing = 0usize;
    if !deduped.is_empty() {
        let mut tx = db.begin().await?;
        for row in &deduped {
            let existed = existing.contains(&row.domain);
            if existed && !overwrite {
                skipped_existing += 1;
                continue;
            }
            if overwrite {
                blocks::upsert(
                    &mut *tx,
                    &row.domain,
                    row.severity,
                    row.reject_media,
                    row.reject_reports,
                    row.obfuscate,
                    &row.private_comment,
                    &row.public_comment,
                    actor_id,
                )
                .await?;
                if existed {
                    updated += 1;
                } else {
                    created += 1;
                }
            } else if blocks::insert(
                &mut *tx,
                &row.domain,
                row.severity,
                row.reject_media,
                row.reject_reports,
                row.obfuscate,
                &row.private_comment,
                &row.public_comment,
                actor_id,
            )
            .await?
            .is_some()
            {
                created += 1;
            } else {
                skipped_existing += 1;
            }
        }
        if created + updated > 0 {
            audit::record(
                &mut *tx,
                Some(actor_id),
                "domain_block.import",
                "domain",
                None,
                json!({
                    "filename": filename,
                    "overwrite": overwrite,
                    "created": created,
                    "updated": updated,
                    "skipped_existing": skipped_existing,
                    "skipped_invalid": issues.len(),
                    "duplicates": duplicates,
                    "total_rows": parsed.total_rows,
                }),
            )
            .await?;
        }
        tx.commit().await?;
    }

    Ok(ImportReport {
        created,
        updated,
        skipped_existing,
        skipped_invalid: issues.len(),
        duplicates,
        total_rows: parsed.total_rows,
        errors: issues,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn parses_mastodon_export_shape() {
        let input = b"#domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate\n\
                      Spam.example,suspend,true,false,\"spam, farming\",true\n\
                      quiet.example,silence,false,true,quiet,\n";
        let parsed = parse_import(input).unwrap();
        assert_eq!(parsed.total_rows, 2);
        assert!(parsed.issues.is_empty());
        assert_eq!(parsed.rows[0].domain, "spam.example");
        assert_eq!(parsed.rows[0].severity, Severity::Suspend);
        assert!(parsed.rows[0].reject_media);
        assert!(!parsed.rows[0].reject_reports);
        assert_eq!(parsed.rows[0].public_comment, "spam, farming");
        assert!(parsed.rows[0].obfuscate);
        assert_eq!(parsed.rows[1].severity, Severity::Silence);
    }

    #[test]
    fn strips_utf8_bom() {
        let parsed = parse_import(b"\xef\xbb\xbfbad.example\n").unwrap();
        assert_eq!(parsed.rows[0].domain, "bad.example");
    }

    #[test]
    fn parses_headerless_domain_lists_as_suspend() {
        let parsed = parse_import(b"bad.example\nhttps://other.example/path\n").unwrap();
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0].domain, "bad.example");
        assert_eq!(parsed.rows[1].domain, "other.example");
        assert!(parsed
            .rows
            .iter()
            .all(|row| row.severity == Severity::Suspend));
    }

    #[test]
    fn parses_unprefixed_headers_and_common_aliases() {
        let input = b"domain,severity,reject_media,private_comment\n\
                      bad.example,reject,yes,private note\n\
                      quiet.example,limit,no,\n";
        let parsed = parse_import(input).unwrap();
        assert!(parsed.issues.is_empty());
        assert_eq!(parsed.rows[0].severity, Severity::Suspend);
        assert!(parsed.rows[0].reject_media);
        assert_eq!(parsed.rows[0].private_comment, "private note");
        assert_eq!(parsed.rows[1].severity, Severity::Silence);
    }

    #[test]
    fn row_errors_do_not_drop_valid_rows() {
        let input = b"#domain,#severity,#reject_media\n\
                      good.example,suspend,true\n\
                      ,suspend,true\n\
                      bad.example,wat,true\n";
        let parsed = parse_import(input).unwrap();
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.issues.len(), 2);
        assert_eq!(parsed.issues[0].line, 3);
        assert!(parsed.issues[1].error.contains("invalid severity"));
    }

    #[test]
    fn missing_severity_defaults_but_blank_severity_is_invalid() {
        let parsed = parse_import(
            b"#domain,#reject_media
default.example,true
",
        )
        .unwrap();
        assert_eq!(parsed.rows[0].severity, Severity::Suspend);
        let parsed = parse_import(
            b"#domain,#severity
blank.example,
",
        )
        .unwrap();
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.issues[0].error, "invalid severity \"\"");
    }

    #[test]
    fn export_uses_mastodon_header_and_columns() {
        let rows = vec![BlockRow {
            id: 1,
            domain: "bad.example".into(),
            severity: Severity::Suspend,
            reject_media: true,
            reject_reports: false,
            obfuscate: true,
            private_comment: "internal".into(),
            public_comment: "public, reason".into(),
            created_by_account_id: None,
            created_at: Utc::now(),
        }];
        let csv = String::from_utf8(export(&rows).unwrap()).unwrap();
        assert_eq!(
            csv,
            "#domain,#severity,#reject_media,#reject_reports,#public_comment,#obfuscate\n\
             bad.example,suspend,true,false,\"public, reason\",true\n"
        );
        assert!(!csv.contains("internal"));
    }
}
