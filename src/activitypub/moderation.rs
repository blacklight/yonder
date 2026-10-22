//! Federation-facing instance moderation — the merge of the config
//! allow/block lists (none today) with `domain_blocks` rows.
//!
//! Only `action='suspend'` blocks federation traffic both ways — the
//! weaker actions (`limit`, `followers_only`, …) change visibility, not
//! transport. Epic 21 owns the moderation surface; this module only
//! reads it.

use sqlx::PgPool;

use crate::config::Config;
use crate::Result;

/// Host part of an actor or inbox URL, lowercased — `None` when the
/// URL doesn't parse.
pub fn actor_domain(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
}

/// Suspended domains from `domain_blocks` (`action='suspend'`).
/// `config` is accepted for symmetry with future config-level lists —
/// there is no `federation.blocked_instances` key today.
pub async fn blocked_domains(pool: &PgPool, _config: &Config) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>("SELECT domain FROM domain_blocks WHERE action = 'suspend'")
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Whether `domain` is currently suspended — checked per inbound
/// activity so new blocks take effect without a restart.
pub async fn is_blocked(pool: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks WHERE domain = $1 AND action = 'suspend')",
    )
    .bind(domain.to_lowercase())
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}
