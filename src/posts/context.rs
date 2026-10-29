//! Thread reads — `GET /api/v1/statuses/{id}/context` (epic 12 follow-up).
//!
//! The Mastodon `Context` entity is `{ancestors, descendants}`: the
//! reply chain above a status (root first) and every visible reply in
//! the subtree below it (flat, chronological — clients re-thread by
//! `in_reply_to_id`).
//!
//! Both directions run as recursive CTEs with the [`can_see`] predicate
//! inlined *inside* the traversal, so the walk itself stops at the
//! first status the viewer can't see — an invisible ancestor breaks the
//! chain (matching Mastodon, where replies under an invisible parent
//! are never reached) instead of being skipped and orphaned.
//!
//! The viewer predicates mirror `timelines::filter`'s shared fragments:
//! author always sees own, `public`/`unlisted` are world-readable,
//! `followers_only` needs an accepted `follows` row or a mention (a
//! mention grants audience — `posts::can_see`), `direct` needs a
//! `status_mentions` row — plus live-author (`visibility::VISIBLE`),
//! instance-suspension and (authenticated viewers) block/mute/
//! domain-block exclusions. `$1` binds the viewer (`NULL` = anonymous,
//! which the predicates degrade to public/unlisted only); `$2` binds
//! the status id.

use sqlx::PgPool;

use crate::{social::visibility, Result};

use super::{row_to_status, StatusRow, STATUS_COLS};

/// Ancestor-walk depth cap — a corrupted `in_reply_to_id` cycle can
/// never spin the recursion.
const ANCESTOR_DEPTH: i64 = 64;
/// Descendant-walk depth cap — the same bound Mastodon applies before
/// it stops expanding a level.
const DESCENDANT_DEPTH: i64 = 20;
/// Total descendant rows returned — bounded so a hostile or runaway
/// thread can't blow up the response.
const DESCENDANT_LIMIT: i64 = 200;

/// `STATUS_COLS` qualified for `s` — the CTEs carry `id`/
/// `in_reply_to_id` too, so the outer SELECT must disambiguate.
fn s_cols() -> String {
    STATUS_COLS
        .split(',')
        .map(|c| format!("s.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The inlined `can_see` + visibility predicate for one traversal
/// branch — aliases `s` (statuses) and `a` (accounts), viewer at `$1`.
fn visible_sql(viewer: Option<i64>) -> String {
    let mut p = String::from(
        "(s.account_id = $1 OR s.visibility IN ('public', 'unlisted') \
         OR (s.visibility IN ('followers_only', 'private') \
             AND (EXISTS (SELECT 1 FROM follows f \
                          WHERE f.account_id = $1 \
                            AND f.target_account_id = s.account_id) \
                  OR EXISTS (SELECT 1 FROM status_mentions sm2 \
                             WHERE sm2.status_id = s.id AND sm2.account_id = $1))) \
         OR (s.visibility = 'direct' \
             AND EXISTS (SELECT 1 FROM status_mentions sm \
                         WHERE sm.status_id = s.id AND sm.account_id = $1)))",
    );
    p.push_str(&format!(" AND ({})", visibility::VISIBLE));
    // Instance moderation: suspended authors' domains vanish
    // everywhere — same unconditional clause timelines apply.
    p.push_str(
        " AND NOT EXISTS (SELECT 1 FROM domain_blocks idb \
         WHERE idb.domain = a.domain AND idb.severity = 'suspend')",
    );
    if viewer.is_some() {
        p.push_str(&format!(
            " AND {} AND {} AND {}",
            visibility::BLOCKS_EXCLUSION,
            visibility::MUTES_EXCLUSION,
            visibility::DOMAIN_BLOCKS_EXCLUSION
        ));
    }
    p
}

/// The visible ancestors of `status_id`, root first. The chain stops
/// at the first missing/tombstoned/invisible ancestor.
pub async fn ancestors(db: &PgPool, status_id: i64, viewer: Option<i64>) -> Result<Vec<StatusRow>> {
    let sql = format!(
        "WITH RECURSIVE up AS ( \
             SELECT s.id, s.in_reply_to_id, 1 AS depth \
             FROM statuses s JOIN accounts a ON a.id = s.account_id \
             WHERE s.id = (SELECT in_reply_to_id FROM statuses WHERE id = $2) \
               AND NOT s.tombstone AND {pred} \
             UNION ALL \
             SELECT s.id, s.in_reply_to_id, up.depth + 1 \
             FROM statuses s JOIN accounts a ON a.id = s.account_id \
             JOIN up ON s.id = up.in_reply_to_id \
             WHERE up.depth < $3 AND NOT s.tombstone AND {pred} \
         ) \
         SELECT {cols} FROM up \
         JOIN statuses s ON s.id = up.id \
         ORDER BY up.depth DESC",
        pred = visible_sql(viewer),
        cols = s_cols(),
    );
    let rows = sqlx::query(&sql)
        .bind(viewer)
        .bind(status_id)
        .bind(ANCESTOR_DEPTH)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(row_to_status).collect())
}

/// The visible replies in `status_id`'s subtree, flat and
/// chronological (`created_at` then `id` — remote replies keep their
/// `published` time, not ingestion order). Replies under invisible
/// parents are pruned with them.
pub async fn descendants(
    db: &PgPool,
    status_id: i64,
    viewer: Option<i64>,
) -> Result<Vec<StatusRow>> {
    let sql = format!(
        "WITH RECURSIVE down AS ( \
             SELECT s.id, 1 AS depth \
             FROM statuses s JOIN accounts a ON a.id = s.account_id \
             WHERE s.in_reply_to_id = $2 AND NOT s.tombstone AND {pred} \
             UNION ALL \
             SELECT s.id, down.depth + 1 \
             FROM statuses s JOIN accounts a ON a.id = s.account_id \
             JOIN down ON s.in_reply_to_id = down.id \
             WHERE down.depth < $3 AND NOT s.tombstone AND {pred} \
         ) \
         SELECT {cols} FROM down \
         JOIN statuses s ON s.id = down.id \
         ORDER BY s.created_at, s.id LIMIT $4",
        pred = visible_sql(viewer),
        cols = s_cols(),
    );
    let rows = sqlx::query(&sql)
        .bind(viewer)
        .bind(status_id)
        .bind(DESCENDANT_DEPTH)
        .bind(DESCENDANT_LIMIT)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(row_to_status).collect())
}
