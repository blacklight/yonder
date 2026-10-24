//! Instance-level moderation: `domain_blocks` queries and the
//! enforcement-facing policy reads.
//!
//! Mastodon's model (migration 0020): `severity` is the enum
//! (`noop`/`silence`/`suspend`), `reject_media` and `reject_reports`
//! are independent booleans that compose with `noop`/`silence` and are
//! redundant under `suspend`. Semantics, matching Mastodon:
//!
//! - `suspend`: no federation traffic either way; the domain's content
//!   and accounts vanish from every surface.
//! - `silence`: content hidden from public timelines/status
//!   search/trends; followers keep it in home; inbound follows become
//!   follow requests; the domain's accounts stay findable via search.
//! - `reject_media`: remote media, avatars, headers and emoji are not
//!   cached or proxied.
//! - `reject_reports`: inbound `Flag`s from the domain are dropped.
//!
//! Reads are live (no caching — a block takes effect immediately).
//! The one snapshot pubby needs (`HandlerOptions.blocked_instances`)
//! is refreshed by [`refresh_blocked`] after every write.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::{http::AppState, Error, Result};

use super::domain::normalize_domain;

/// `domain_blocks` severities — serialized lowercase (DB CHECK and
/// the Mastodon `Admin::DomainBlock` field both use these strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Reject-only block — no account-level restriction.
    Noop,
    /// Mastodon's "limit": hidden publicly, visible to followers.
    Silence,
    /// Defederation — no traffic, no content.
    Suspend,
}

impl Severity {
    /// The DB/API spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Noop => "noop",
            Severity::Silence => "silence",
            Severity::Suspend => "suspend",
        }
    }

    /// Parse the DB/API spelling; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "noop" => Some(Severity::Noop),
            "silence" | "limit" => Some(Severity::Silence),
            "suspend" => Some(Severity::Suspend),
            _ => None,
        }
    }
}

/// A `domain_blocks` row.
#[derive(Debug, Clone)]
pub struct BlockRow {
    pub id: i64,
    pub domain: String,
    pub severity: Severity,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub obfuscate: bool,
    pub private_comment: String,
    pub public_comment: String,
    pub created_by_account_id: Option<i64>,
    pub created_at: DateTime<Utc>,
}

fn block_row(r: &sqlx::postgres::PgRow) -> BlockRow {
    BlockRow {
        id: r.get("id"),
        domain: r.get("domain"),
        severity: Severity::parse(&r.get::<String, _>("severity")).unwrap_or(Severity::Suspend),
        reject_media: r.get("reject_media"),
        reject_reports: r.get("reject_reports"),
        obfuscate: r.get("obfuscate"),
        private_comment: r.get("private_comment"),
        public_comment: r.get("public_comment"),
        created_by_account_id: r.get("created_by_account_id"),
        created_at: r.get("created_at"),
    }
}

const BLOCK_COLS: &str = "id, domain, severity, reject_media, reject_reports, obfuscate, \
     private_comment, public_comment, created_by_account_id, created_at";

/// Create or update the block for `domain` (already normalized).
/// Returns the stored row. `db` is any sqlx executor — the routes
/// pass `&mut *tx` to commit the write atomically with its audit row.
/// The argument list is Mastodon's `Admin::DomainBlock` shape — a
/// struct would just re-wrap it.
#[allow(clippy::too_many_arguments)]
pub async fn upsert<'e, E>(
    db: E,
    domain: &str,
    severity: Severity,
    reject_media: bool,
    reject_reports: bool,
    obfuscate: bool,
    private_comment: &str,
    public_comment: &str,
    created_by: i64,
) -> Result<BlockRow>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if domain.is_empty() {
        return Err(Error::Validation("invalid domain".into()));
    }
    let row = sqlx::query(&format!(
        "INSERT INTO domain_blocks \
             (domain, severity, reject_media, reject_reports, obfuscate, \
              private_comment, public_comment, created_by_account_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (domain) DO UPDATE SET \
             severity = EXCLUDED.severity, \
             reject_media = EXCLUDED.reject_media, \
             reject_reports = EXCLUDED.reject_reports, \
             obfuscate = EXCLUDED.obfuscate, \
             private_comment = EXCLUDED.private_comment, \
             public_comment = EXCLUDED.public_comment, \
             updated_at = now() \
         RETURNING {BLOCK_COLS}"
    ))
    .bind(domain)
    .bind(severity.as_str())
    .bind(reject_media)
    .bind(reject_reports)
    .bind(obfuscate)
    .bind(private_comment)
    .bind(public_comment)
    .bind(created_by)
    .fetch_one(db)
    .await?;
    Ok(block_row(&row))
}

/// Load a block by normalized domain.
pub async fn policy_for(db: &PgPool, domain: &str) -> Result<Option<BlockRow>> {
    let row = sqlx::query(&format!(
        "SELECT {BLOCK_COLS} FROM domain_blocks WHERE domain = $1"
    ))
    .bind(normalize_domain(domain))
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(block_row))
}

/// Whether `domain` (normalized inside) is `severity='suspend'`.
pub async fn is_suspended(db: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks WHERE domain = $1 AND severity = 'suspend')",
    )
    .bind(normalize_domain(domain))
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `domain` (normalized inside) is `severity='silence'`.
pub async fn is_silenced(db: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks WHERE domain = $1 AND severity = 'silence')",
    )
    .bind(normalize_domain(domain))
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `domain` rejects remote media (`reject_media` or full
/// `suspend` — a suspended domain stores nothing either way).
pub async fn rejects_media(db: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks WHERE domain = $1 \
         AND (reject_media OR severity = 'suspend'))",
    )
    .bind(normalize_domain(domain))
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `domain` rejects inbound reports (`reject_reports` or
/// full `suspend`).
pub async fn rejects_reports(db: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks WHERE domain = $1 \
         AND (reject_reports OR severity = 'suspend'))",
    )
    .bind(normalize_domain(domain))
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Load a block by id.
pub async fn get(db: &PgPool, id: i64) -> Result<BlockRow> {
    let row = sqlx::query(&format!(
        "SELECT {BLOCK_COLS} FROM domain_blocks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("domain block".into()))?;
    Ok(block_row(&row))
}

/// All blocks, newest first (admin listing; keyset pagination on id).
pub async fn list(
    db: &PgPool,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<BlockRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {BLOCK_COLS} FROM domain_blocks \
         WHERE ($1::bigint IS NULL OR id < $1) \
           AND ($2::bigint IS NULL OR id > $2) \
         ORDER BY id DESC LIMIT $3"
    ))
    .bind(max_id)
    .bind(since_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(block_row).collect())
}

/// The public moderated-instances view — severity + public comment,
/// domain obfuscated when the row asks for it.
#[derive(Debug)]
pub struct PublicBlock {
    pub domain: String,
    pub severity: Severity,
    pub public_comment: String,
}

/// Public listing, honouring per-block `obfuscate`.
pub async fn list_public(db: &PgPool) -> Result<Vec<PublicBlock>> {
    Ok(list(db, None, None, 10_000)
        .await?
        .into_iter()
        .map(|b| PublicBlock {
            domain: if b.obfuscate {
                obfuscate_domain(&b.domain)
            } else {
                b.domain
            },
            severity: b.severity,
            public_comment: b.public_comment,
        })
        .collect())
}

/// Delete a block by id. Returns `false` when absent. `db` is any
/// sqlx executor — the route passes `&mut *tx` to commit with audit.
pub async fn delete<'e, E>(db: E, id: i64) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    Ok(sqlx::query("DELETE FROM domain_blocks WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected()
        > 0)
}

/// Mastodon's domain obfuscation (`DomainBlock#public_domain`): keep
/// the first and last `len/4` characters, mask non-`.` chars between.
pub fn obfuscate_domain(domain: &str) -> String {
    let len = domain.chars().count();
    let visible = len / 4;
    domain
        .chars()
        .enumerate()
        .map(|(i, c)| {
            if i > visible && i < len - visible && c != '.' {
                '*'
            } else {
                c
            }
        })
        .collect()
}

/// Rebuild the suspended-domain snapshot pubby's handlers hold
/// (`ApCore.blocked` feeds `HandlerOptions.blocked_instances`, which
/// gates outbound fetches and the inbox fan-out) and drop cached
/// handlers so rebuilt options pick the new list up. Called after
/// every domain-block write; SQL-facing reads stay live and don't
/// need this.
pub async fn refresh_blocked(state: &AppState) -> Result<()> {
    let Some(ap) = state.ap.as_ref() else {
        return Ok(());
    };
    let list = crate::activitypub::moderation::blocked_domains(&state.db, &state.config()).await?;
    *ap.core.blocked.write().unwrap_or_else(|e| e.into_inner()) = list;
    crate::activitypub::actors::invalidate_handlers(&ap.handlers, None);
    Ok(())
}

/// Build the `follow_policy` closure bound on per-user handlers:
/// inbound Follows from silenced domains (or targeting locked
/// accounts — supplying a policy shadows pubby's locked default, so
/// it is reproduced here) resolve to `Manual` (a stored
/// `follow_requests` row and no `Accept`), everything else passes
/// `None` → pubby's `Accept`.
pub fn silenced_follow_policy(
    core: &Arc<crate::activitypub::ApCore>,
) -> pubby_sdk::handlers::FollowPolicyFn {
    let core = core.clone();
    Arc::new(move |target_actor_id: &str, follower_actor_id: &str| {
        let core = core.clone();
        let target = target_actor_id.to_string();
        let follower = follower_actor_id.to_string();
        crate::activitypub::actors::sync_block(async move {
            let silenced = match crate::activitypub::moderation::actor_domain(&follower) {
                Some(domain) => is_silenced(&core.db, &domain).await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, domain, "silence check failed; allowing follow");
                    false
                }),
                None => false,
            };
            if silenced {
                return Some(pubby_sdk::FollowPolicy::Manual);
            }
            let locked: bool = sqlx::query_scalar(
                "SELECT COALESCE(locked, false) FROM accounts WHERE actor_id = $1",
            )
            .bind(&target)
            .fetch_optional(&core.db)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);
            if locked {
                Some(pubby_sdk::FollowPolicy::Manual)
            } else {
                None
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obfuscation_masks_middle_chars() {
        // Mastodon: visible = len/4; mask non-dot chars strictly
        // between the first and last `visible` chars.
        // "example.com" (len 11, visible 2) masks indices 3..8.
        assert_eq!(obfuscate_domain("example.com"), "exa****.*om");
        // Short domains (visible = 0) keep only the first char + dots.
        assert_eq!(obfuscate_domain("a.b"), "a.*");
        assert_eq!(obfuscate_domain("ab"), "a*");
    }

    #[test]
    fn severity_parses() {
        assert_eq!(Severity::parse("silence"), Some(Severity::Silence));
        assert_eq!(Severity::parse("limit"), Some(Severity::Silence));
        assert_eq!(Severity::parse("noop"), Some(Severity::Noop));
        assert_eq!(Severity::parse("suspend"), Some(Severity::Suspend));
        assert_eq!(Severity::parse("bogus"), None);
    }
}
