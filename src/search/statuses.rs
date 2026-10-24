//! Status search — trigram ILIKE over `content`/`spoiler_text`
//! (indexes from migration 0003) with the caller's visibility and the
//! author's `include_public_posts_in_search` opt-out applied.

use sqlx::PgPool;

use crate::posts::{row_to_status, StatusRow};
use crate::social::visibility;
use crate::timelines::filter::Bind;
use crate::timelines::query::{bind_all, select_cols};
use crate::{Error, Result};

use super::like_pattern;
use super::params::{QueryKind, SearchParams};

/// Status visibility predicate for `s`/`a` joins — `$1` is the viewer
/// when bound. Anonymous callers see `public`/`unlisted` only.
/// `pub(crate)` — hashtag and resolve paths reuse the same rule.
pub(crate) fn visibility_clause(viewer: Option<i64>) -> String {
    match viewer {
        None => "s.visibility IN ('public', 'unlisted')".to_string(),
        Some(_) => "(s.visibility IN ('public', 'unlisted') \
            OR s.account_id = $1 \
            OR (s.visibility IN ('followers_only', 'private') \
                AND EXISTS (SELECT 1 FROM follows fv \
                            WHERE fv.account_id = $1 \
                              AND fv.target_account_id = s.account_id)) \
            OR (s.visibility = 'direct' \
                AND EXISTS (SELECT 1 FROM status_mentions sm \
                            WHERE sm.status_id = s.id \
                              AND sm.account_id = $1)))"
            .to_string(),
    }
}

/// The WHERE parts every status-scoped search query shares: live
/// author, no tombstones, moderation exclusions, caller visibility.
/// `pub(crate)` — `hashtags` composes the same set.
pub(crate) fn base_clauses(
    viewer: Option<i64>,
    binds: &mut Vec<Bind>,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Vec<String> {
    if let Some(v) = viewer {
        binds.push(Bind::I64(v)); // $1 — the *viewer* convention
    }
    let mut clauses = vec![
        "NOT s.tombstone".to_string(),
        format!("({})", visibility::VISIBLE),
        // Instance moderation: suspended domains vanish everywhere;
        // limited/timeline-removed domains are hidden in search just
        // like in the public scopes (filter.rs treats them alike).
        "NOT EXISTS (SELECT 1 FROM domain_blocks idb \
         WHERE idb.domain = a.domain AND idb.action = 'suspend')"
            .to_string(),
        "NOT EXISTS (SELECT 1 FROM domain_blocks idb \
         WHERE idb.domain = a.domain \
           AND idb.action IN ('limit', 'remove_from_timeline'))"
            .to_string(),
        visibility_clause(viewer),
    ];
    if viewer.is_some() {
        clauses.push(visibility::BLOCKS_EXCLUSION.to_string());
        clauses.push(visibility::MUTES_EXCLUSION.to_string());
        clauses.push(visibility::DOMAIN_BLOCKS_EXCLUSION.to_string());
    }
    if let Some(hide) = hide {
        let (sql, hb) = hide.numbered(binds.len());
        binds.extend(hb);
        clauses.push(sql);
    }
    clauses
}

/// Full-text match over statuses. Only `Text` queries reach here —
/// `Url` is a lookup (resolve.rs), `Hashtag` lives in `hashtags.rs`.
/// Status matching is always substring (`%{q}%`); prefix mode is an
/// account/tag autocomplete signal, not a status one.
pub async fn search(
    db: &PgPool,
    viewer: Option<i64>,
    params: &SearchParams,
    kind: &QueryKind,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Result<Vec<StatusRow>> {
    let QueryKind::Text(text) = kind else {
        return Ok(Vec::new());
    };
    if let Some(author) = params.account_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1)")
                .bind(author)
                .fetch_one(db)
                .await?;
        if !exists {
            return Err(Error::NotFound("account".to_string()));
        }
    }

    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses = base_clauses(viewer, &mut binds, hide);
    let mut alloc = |b: Bind| {
        binds.push(b);
        format!("${}", binds.len())
    };

    let pat = alloc(Bind::Text(like_pattern(text, false)));
    clauses.push(format!(
        "(s.content ILIKE {pat} OR s.spoiler_text ILIKE {pat})"
    ));
    // Reblogs carry the original's content — the original is
    // independently searchable, so doubles would be noise.
    clauses.push("s.reblog_of_id IS NULL".to_string());
    // Author opt-out: the author's own posts stay findable to them.
    clauses.push(match viewer {
        Some(_) => "(COALESCE(u.include_public_posts_in_search, true) \
                    OR s.account_id = $1)"
            .to_string(),
        None => "COALESCE(u.include_public_posts_in_search, true)".to_string(),
    });
    if let Some(author) = params.account_id {
        let p = alloc(Bind::I64(author));
        clauses.push(format!("s.account_id = {p}"));
    }

    let limit = alloc(Bind::I64(params.limit));
    let offset = alloc(Bind::I64(params.offset));
    let sql = format!(
        "SELECT {} FROM statuses s \
         JOIN accounts a ON a.id = s.account_id \
         LEFT JOIN users u ON u.account_id = a.id \
         WHERE {} ORDER BY s.id DESC LIMIT {limit} OFFSET {offset}",
        select_cols(),
        clauses.join(" AND ")
    );
    let q = bind_all(sqlx::query(&sql), binds.iter());
    let rows = q.fetch_all(db).await?;
    Ok(rows.iter().map(row_to_status).collect())
}

/// `statuses.id` → row when `viewer` may see it (same base clauses as
/// search, no text match). `resolve.rs` uses it to gate fetched and
/// known remote statuses.
pub(crate) async fn visible_row(
    db: &PgPool,
    viewer: Option<i64>,
    status_id: i64,
    hide: Option<&crate::timelines::filter::HideClause>,
) -> Result<Option<StatusRow>> {
    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses = base_clauses(viewer, &mut binds, hide);
    binds.push(Bind::I64(status_id));
    clauses.push(format!("s.id = ${}", binds.len()));
    let sql = format!(
        "SELECT {} FROM statuses s \
         JOIN accounts a ON a.id = s.account_id \
         LEFT JOIN users u ON u.account_id = a.id \
         WHERE {}",
        select_cols(),
        clauses.join(" AND ")
    );
    let q = bind_all(sqlx::query(&sql), binds.iter());
    let row = q.fetch_optional(db).await?;
    Ok(row.as_ref().map(row_to_status))
}
