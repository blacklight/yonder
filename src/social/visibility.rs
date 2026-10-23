//! Shared visibility predicates for the social graph.
//!
//! Listings bind the viewer as `$1` and join `accounts a` — the `*_EXCLUSION`
//! fragments interpolate into those queries. Epic 15 (timelines),
//! epic 16 (notifications) and epic 21 (instance blocks) reuse them.

use sqlx::PgPool;

use crate::Result;

/// No `blocks` row in either direction between the viewer (`$1`) and
/// the joined account (`a.id`). Expects alias `a`.
pub const BLOCKS_EXCLUSION: &str = "NOT EXISTS (SELECT 1 FROM blocks b \
    WHERE (b.account_id = $1 AND b.target_account_id = a.id) \
       OR (b.account_id = a.id AND b.target_account_id = $1))";

/// No unexpired `mutes` row from the viewer (`$1`) to the joined
/// account (`a.id`). Expects alias `a`.
pub const MUTES_EXCLUSION: &str = "NOT EXISTS (SELECT 1 FROM mutes m \
    WHERE m.account_id = $1 AND m.target_account_id = a.id \
      AND (m.expires_at IS NULL OR m.expires_at > now()))";

/// The joined account's domain (`a.domain`) is not in the viewer's
/// (`$1`) `account_domain_blocks`. Expects alias `a`.
pub const DOMAIN_BLOCKS_EXCLUSION: &str = "NOT EXISTS (SELECT 1 FROM account_domain_blocks d \
    WHERE d.account_id = $1 AND d.domain = a.domain)";

/// The joined account is live — not suspended and not deleted.
/// Expects alias `a`.
pub const VISIBLE: &str = "a.suspended_at IS NULL AND a.deleted_at IS NULL";

/// `a` is a remote account (domain set). Local accounts are always
/// domain-NULL in `accounts` — this is how listings distinguish rows
/// that triggers and remote fetches own.
pub const REMOTE: &str = "a.domain IS NOT NULL";

/// Whether `a` and `b` are blocked in either direction.
pub async fn pair_blocked(db: &PgPool, a: i64, b: i64) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM blocks \
         WHERE (account_id = $1 AND target_account_id = $2) \
            OR (account_id = $2 AND target_account_id = $1))",
    )
    .bind(a)
    .bind(b)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `viewer` currently mutes `target` (unexpired rows only).
pub async fn muted(db: &PgPool, viewer: i64, target: i64) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM mutes \
         WHERE account_id = $1 AND target_account_id = $2 \
           AND (expires_at IS NULL OR expires_at > now()))",
    )
    .bind(viewer)
    .bind(target)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `viewer` has a per-user domain block on `domain` (empty or
/// NULL `domain` never matches — local accounts can't be
/// domain-blocked).
pub async fn domain_blocked(db: &PgPool, viewer: i64, domain: Option<&str>) -> Result<bool> {
    let Some(domain) = domain else {
        return Ok(false);
    };
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM account_domain_blocks \
         WHERE account_id = $1 AND domain = $2)",
    )
    .bind(viewer)
    .bind(domain.to_lowercase())
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether the account is live — not suspended and not deleted.
pub async fn visible(db: &PgPool, account_id: i64) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts \
         WHERE id = $1 AND suspended_at IS NULL AND deleted_at IS NULL)",
    )
    .bind(account_id)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}
