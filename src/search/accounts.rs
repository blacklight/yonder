//! Account search — trigram ILIKE over `username`/`display_name`
//! (indexes from migration 0018) and `domain`, with the social-graph
//! exclusions and the `discoverable` opt-out applied.

use sqlx::{PgPool, Row};

use crate::posts::mentions::plausible_domain;
use crate::social::visibility;
use crate::timelines::filter::Bind;
use crate::timelines::query::bind_all;
use crate::Result;

use super::params::{is_prefix_mode, QueryKind, SearchParams};
use super::{escape_like, like_pattern};

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
        // Unfetched stubs for phantom remote domains don't surface in
        // search — only established remote actors (or fresh stubs
        // still awaiting their first fetch).
        crate::activitypub::storage::established_remote_clause("a"),
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
    search_inner(db, viewer, params, kind, None).await
}

/// The ES-backend variant: identical filters and ordering, but the
/// match clause is replaced by `a.id = ANY(ids)` — the index decided
/// *what* matches, Postgres decides *who may see it*. Only `Text`
/// kinds reach here (FQN stays an exact PG match); an empty candidate
/// set short-circuits without touching the DB.
pub async fn search_ids(
    db: &PgPool,
    viewer: Option<i64>,
    params: &SearchParams,
    kind: &QueryKind,
    ids: &[i64],
) -> Result<Vec<i64>> {
    if ids.is_empty() || !matches!(kind, QueryKind::Text(_)) {
        return Ok(Vec::new());
    }
    search_inner(db, viewer, params, kind, Some(ids)).await
}

async fn search_inner(
    db: &PgPool,
    viewer: Option<i64>,
    params: &SearchParams,
    kind: &QueryKind,
    ids: Option<&[i64]>,
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
            if let Some(ids) = ids {
                let p = alloc(Bind::I64s(ids.to_vec()));
                clauses.push(format!("a.id = ANY({p})"));
            }
            // Mastodon strips a leading '@' from account queries.
            let t = text.strip_prefix('@').unwrap_or(text);
            let prefix = is_prefix_mode(params, kind);
            let mut match_clauses = Vec::new();
            if ids.is_none() {
                if prefix {
                    // Autocomplete: case-insensitive prefix over username and
                    // display name. `lower(col) LIKE` uses the btree pattern
                    // indexes from migration 0056 — trigram indexes don't
                    // help very short prefixes.
                    let p = alloc(Bind::Text(format!("{}%", escape_like(&t.to_lowercase()))));
                    match_clauses.push(format!("lower(a.username) LIKE {p}"));
                    match_clauses.push(format!("lower(a.display_name) LIKE {p}"));
                } else {
                    let pat = alloc(Bind::Text(like_pattern(t, false)));
                    match_clauses.push(format!("a.username ILIKE {pat}"));
                    match_clauses.push(format!("a.display_name ILIKE {pat}"));
                    // A bare plausible domain (`example.com`) matches the
                    // actor's home instance exactly. Substring domain search
                    // is deliberately not done: at fediverse scale it matches
                    // hundreds of thousands of cached remote actors.
                    if plausible_domain(t) {
                        let d = alloc(Bind::Text(t.to_lowercase()));
                        match_clauses.push(format!("a.domain = {d}"));
                    }
                }
                clauses.push(format!("({})", match_clauses.join(" OR ")));
            }
            // Non-exact matches respect the discoverability opt-out —
            // except accounts the caller already follows.
            // `users.indexable` deliberately does not apply: it governs
            // search-engine indexing (robots meta, feeds), not the
            // instance's own search — `discoverable` is the opt-out here.
            let mut disc = "a.discoverable".to_string();
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
         WHERE {} ORDER BY {order} LIMIT {limit} OFFSET {offset}",
        clauses.join(" AND ")
    );
    let q = bind_all(sqlx::query(&sql), binds.iter());
    let rows = q.fetch_all(db).await?;
    Ok(rows.iter().map(|r| r.get::<i64, _>("id")).collect())
}
