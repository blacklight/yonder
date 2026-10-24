//! Account search — trigram ILIKE over `username`/`display_name`
//! (indexes from migration 0018) and `domain`, with the social-graph
//! exclusions and the `discoverable`/`indexable` opt-outs applied.

use sqlx::{PgPool, Row};

use crate::social::visibility;
use crate::timelines::filter::Bind;
use crate::timelines::query::bind_all;
use crate::Result;

use super::like_pattern;
use super::params::{is_prefix_mode, QueryKind, SearchParams};

/// Predicates every account match shares: live account, no suspended
/// instance domain, and the viewer's block/mute/domain-block
/// exclusions. `$1` is the viewer when bound.
fn base_clauses(viewer: Option<i64>, binds: &mut Vec<Bind>) -> Vec<String> {
    let mut clauses = vec![
        format!("({})", visibility::VISIBLE),
        // Suspended instance domains vanish everywhere.
        "NOT EXISTS (SELECT 1 FROM domain_blocks idb \
         WHERE idb.domain = a.domain AND idb.severity = 'suspend')"
            .to_string(),
    ];
    if let Some(v) = viewer {
        binds.push(Bind::I64(v)); // $1 — the *viewer* convention
        clauses.push(visibility::BLOCKS_EXCLUSION.to_string());
        clauses.push(visibility::MUTES_EXCLUSION.to_string());
        clauses.push(visibility::DOMAIN_BLOCKS_EXCLUSION.to_string());
    }
    clauses
}

/// Whether `account_id` passes the same base rules as search matches.
/// Exact URL/FQN hits use this instead of search: they skip the
/// discoverability opt-outs, but never the social-graph exclusions.
pub(crate) async fn visible(db: &PgPool, viewer: Option<i64>, account_id: i64) -> Result<bool> {
    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses = base_clauses(viewer, &mut binds);
    binds.push(Bind::I64(account_id));
    clauses.push(format!("a.id = ${}", binds.len()));
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM accounts a WHERE {})",
        clauses.join(" AND ")
    );
    let row = bind_all(sqlx::query(&sql), binds.iter())
        .fetch_one(db)
        .await?;
    Ok(row.get::<bool, _>(0))
}

/// Ordered `accounts.id` page for `kind`. `Fqn` is an exact
/// case-insensitive match (local `domain IS NULL` never matches a
/// remote-shaped query); `Text` is a substring (or prefix, in
/// autocomplete mode) match over username, display name and domain —
/// `a.domain` catches "search a whole remote instance" queries.
/// `Url`/`Hashtag` kinds never reach here (handled elsewhere) and
/// return empty.
pub async fn search(
    db: &PgPool,
    viewer: Option<i64>,
    params: &SearchParams,
    kind: &QueryKind,
) -> Result<Vec<i64>> {
    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses = base_clauses(viewer, &mut binds);
    let mut alloc = |b: Bind| {
        binds.push(b);
        format!("${}", binds.len())
    };

    let text_for_rank = match kind {
        QueryKind::Fqn { username, domain } => {
            let u = alloc(Bind::Text(username.clone()));
            let d = alloc(Bind::Text(domain.clone()));
            clauses.push(format!(
                "lower(a.username) = lower({u}) AND lower(a.domain) = lower({d})"
            ));
            username.clone()
        }
        QueryKind::Text(text) => {
            // Mastodon strips a leading '@' from account queries.
            let t = text.strip_prefix('@').unwrap_or(text);
            let pat = alloc(Bind::Text(like_pattern(t, is_prefix_mode(params, kind))));
            clauses.push(format!(
                "(a.username ILIKE {pat} \
                 OR a.display_name ILIKE {pat} \
                 OR a.domain ILIKE {pat})"
            ));
            // Non-exact matches respect the discoverability opt-outs —
            // except accounts the caller already follows.
            let mut disc = "(a.discoverable AND COALESCE(u.indexable, true))".to_string();
            if viewer.is_some() {
                disc = format!(
                    "({disc} OR EXISTS (SELECT 1 FROM follows ff \
                     WHERE ff.account_id = $1 AND ff.target_account_id = a.id))"
                );
            }
            clauses.push(disc);
            t.to_string()
        }
        _ => return Ok(Vec::new()),
    };
    if params.following && viewer.is_some() {
        clauses.push(
            "EXISTS (SELECT 1 FROM follows ff \
             WHERE ff.account_id = $1 AND ff.target_account_id = a.id)"
                .to_string(),
        );
    }

    let order = match kind {
        QueryKind::Fqn { .. } => "a.id".to_string(),
        _ => {
            let q = alloc(Bind::Text(text_for_rank));
            format!(
                "(lower(a.username) = lower({q})) DESC, \
                 (a.domain IS NULL) DESC, \
                 similarity(a.username, {q}) \
                   + similarity(COALESCE(a.display_name, ''), {q}) DESC, \
                 a.id"
            )
        }
    };

    let limit = alloc(Bind::I64(params.limit));
    let offset = alloc(Bind::I64(params.offset));
    let sql = format!(
        "SELECT a.id FROM accounts a \
         LEFT JOIN users u ON u.account_id = a.id \
         WHERE {} ORDER BY {order} LIMIT {limit} OFFSET {offset}",
        clauses.join(" AND ")
    );
    let q = bind_all(sqlx::query(&sql), binds.iter());
    let rows = q.fetch_all(db).await?;
    Ok(rows.iter().map(|r| r.get::<i64, _>("id")).collect())
}
