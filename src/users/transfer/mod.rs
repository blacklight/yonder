//! Archive import/export (epic 11): Mastodon-compatible CSV/JSON
//! kinds, the full `archive` zip, and the `Import`/`Export`/
//! `ExportCleanup` job handlers.
//!
//! - Standalone CSV/JSON exports are generated inline (DB-only,
//!   bounded) and returned as file downloads. The full `archive` is a
//!   zip produced by the `Export` job under the private `exports/`
//!   prefix, downloadable through an authenticated endpoint.
//! - Every import runs through the `Import` job — the upload request
//!   only validates and stores the file. Handlers work in bounded
//!   batches (`BATCH_ROWS` or `BATCH_SECONDS` per run) and re-enqueue
//!   with a persisted cursor so large archives stay well under
//!   `jobs.handler_timeout_seconds`.
//! - Imported posts never federate: they get fresh local URIs with the
//!   source id in `statuses.original_id` (deduped by
//!   `uix_statuses_account_original`) and the source timestamp in
//!   `original_created_at`/`created_at`.

use crate::{activitypub::ApContext, jobs::JobKind, Error, Result};

/// Mastodon's `Form::Import::ROWS_PROCESSING_LIMIT`.
const ROWS_LIMIT: i64 = 20_000;

/// Rows applied per `Import` run before re-enqueueing.
const BATCH_ROWS: i64 = 500;

/// Wall-clock budget per `Import` run — under the 240 s handler timeout.
/// Progress and the `updated_at` heartbeat are written at each batch
/// boundary and after every overwrite removal, so a silent gap stays far
/// below the sweep's 30-minute stale threshold.
const BATCH_SECONDS: u64 = 60;

/// Cursor multiplier separating sub-kinds inside an archive import.
const KIND_OFFSET: i64 = 1_000_000_000;

/// Export download expiry (Mastodon's archive cooldown/validity).
const EXPORT_TTL_DAYS: i64 = 7;

const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The transferable kinds. `Archive` is the full zip; `Followers` is
/// export-only (a remote can't be made to follow you by importing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `following_accounts.csv`
    Following,
    /// `followers.csv` — export only.
    Followers,
    /// `blocked_accounts.csv`
    Blocking,
    /// `muted_accounts.csv`
    Muting,
    /// `domain_blocks.csv`
    DomainBlocks,
    /// `bookmarks.csv` / `bookmarks.json` in archives
    Bookmarks,
    /// `lists.csv`
    Lists,
    /// `filters.json` — a `yonder` JSON shape (Mastodon has no format).
    Filters,
    /// The full zip archive.
    Archive,
}

impl Kind {
    /// API name used in the `kind` column and request bodies.
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Following => "following",
            Kind::Followers => "followers",
            Kind::Blocking => "blocking",
            Kind::Muting => "muting",
            Kind::DomainBlocks => "domain_blocks",
            Kind::Bookmarks => "bookmarks",
            Kind::Lists => "lists",
            Kind::Filters => "filters",
            Kind::Archive => "archive",
        }
    }

    /// Parse an API name.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "following" => Kind::Following,
            "followers" => Kind::Followers,
            "blocking" => Kind::Blocking,
            "muting" => Kind::Muting,
            "domain_blocks" => Kind::DomainBlocks,
            "bookmarks" => Kind::Bookmarks,
            "lists" => Kind::Lists,
            "filters" => Kind::Filters,
            "archive" => Kind::Archive,
            other => {
                return Err(Error::Validation(format!(
                    "unknown import/export kind {other:?}"
                )))
            }
        })
    }

    /// The Mastodon file name for this kind inside an archive.
    pub fn file_name(&self) -> &'static str {
        match self {
            Kind::Following => "following_accounts.csv",
            Kind::Followers => "followers.csv",
            Kind::Blocking => "blocked_accounts.csv",
            Kind::Muting => "muted_accounts.csv",
            Kind::DomainBlocks => "blocked_domains.csv",
            Kind::Bookmarks => "bookmarks.csv",
            Kind::Lists => "lists.csv",
            Kind::Filters => "custom_filters.json",
            Kind::Archive => "archive.zip",
        }
    }

    /// Whether this kind is importable.
    pub fn importable(&self) -> bool {
        !matches!(self, Kind::Followers)
    }

    /// The OAuth scope required to export this kind.
    pub fn read_scope(&self) -> &'static str {
        match self {
            Kind::Following | Kind::Followers => "read:follows",
            Kind::Blocking => "read:blocks",
            Kind::Muting => "read:mutes",
            Kind::Bookmarks => "read:bookmarks",
            Kind::Lists => "read:lists",
            Kind::Filters => "read:filters",
            Kind::DomainBlocks => "read:accounts",
            // The archive contains every category — it carries its own
            // `read:backups` scope (the aggregate-scope shape is a
            // recorded contract deviation).
            Kind::Archive => "read:backups",
        }
    }

    /// The OAuth scope required to import this kind.
    pub fn write_scope(&self) -> &'static str {
        match self {
            Kind::Following => "write:follows",
            Kind::Blocking => "write:blocks",
            Kind::Muting => "write:mutes",
            Kind::Bookmarks => "write:bookmarks",
            Kind::Lists => "write:lists",
            Kind::Filters => "write:filters",
            Kind::DomainBlocks | Kind::Archive | Kind::Followers => "write:accounts",
        }
    }
}

/// `user@domain` for remote accounts, `user@localdomain` on export
/// (Mastodon appends the local domain so the file travels).
fn render_acct(username: &str, domain: Option<&str>, local_domain: &str) -> String {
    match domain {
        Some(d) => format!("{username}@{d}"),
        None => format!("{username}@{local_domain}"),
    }
}

/// Normalize a file `acct` for comparison/dedupe: lowercase, and strip
/// the local domain so `alice@localhost` matches `alice`.
fn normalize_acct(acct: &str, local_domain: &str) -> String {
    let acct = acct.trim().trim_start_matches('@').to_lowercase();
    match acct.split_once('@') {
        Some((user, domain)) if domain == local_domain => user.to_string(),
        _ => acct,
    }
}

pub mod archive;
pub(crate) mod crypto;
mod csv;
pub mod export;
mod import;
mod publish;
pub mod remote;
mod restore;

pub use csv::export_kind;
pub use export::{ExportCleanupJob, ExportJob};
pub use import::{count_rows, ImportJob, ImportSweepJob};
pub use publish::ImportPublishJob;
pub(crate) use restore::str_list;

/// Register this module's job handlers.
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    registry.register(JobKind::Import, ImportJob::new(ap.cloned()));
    registry.register(JobKind::Export, ExportJob::new(ap.cloned()));
    registry.register(JobKind::ExportCleanup, ExportCleanupJob);
    registry.register(JobKind::ImportSweep, ImportSweepJob);
    if let Some(ap) = ap {
        registry.register(
            JobKind::ImportPublish,
            publish::ImportPublishJob::new(ap.clone()),
        );
    }
}
