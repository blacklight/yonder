//! Conversation mutes — `POST /api/v1/statuses/{id}/mute|unmute`.
//! Mastodon mutes whole threads: the row keys on the thread's *root*
//! status (resolved once at mute time), so a reply mute silences
//! notifications for the entire conversation. Read paths (status
//! `muted` flag, notifications list) resolve roots for a batch with
//! one recursive walk each.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;

use crate::{
    http::middleware::auth::AuthContext,
    posts::{PostContext, StatusRow},
    Error, Result,
};

use super::load_visible;

/// Resolve thread roots for a batch of status ids — one recursive
/// walk; `orig → root`. A status whose parent chain dead-ends
/// (remote parent never materialized) resolves to the topmost known
/// ancestor; a root resolves to itself. Depth-capped against cyclic
/// `in_reply_to` graphs (remote ids can point anywhere).
pub async fn roots_for(db: &PgPool, ids: &[i64]) -> Result<HashMap<i64, i64>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (i64, i64)>(
        "WITH RECURSIVE walk(orig, cur, parent, depth) AS ( \
             SELECT id, id, in_reply_to_id, 0 FROM statuses WHERE id = ANY($1) \
             UNION ALL \
             SELECT w.orig, s.id, s.in_reply_to_id, w.depth + 1 \
             FROM walk w JOIN statuses s ON s.id = w.parent \
             WHERE w.depth < 128 \
         ) \
         SELECT DISTINCT ON (orig) orig, cur FROM walk \
         ORDER BY orig, depth DESC",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The viewer's muted roots intersected with `roots` — `orig`s whose
/// root is muted.
fn muted_ids(roots: &HashMap<i64, i64>, muted_roots: &HashSet<i64>) -> HashSet<i64> {
    roots
        .iter()
        .filter(|(_, root)| muted_roots.contains(*root))
        .map(|(orig, _)| *orig)
        .collect()
}

/// Ids from `ids` that sit inside conversations the viewer muted —
/// batch helper for the `muted` flag and notification filtering.
pub async fn muted_status_ids(
    db: &PgPool,
    viewer: Option<i64>,
    ids: &[i64],
) -> Result<HashSet<i64>> {
    let (Some(viewer), false) = (viewer, ids.is_empty()) else {
        return Ok(HashSet::new());
    };
    let roots = roots_for(db, ids).await?;
    if roots.is_empty() {
        return Ok(HashSet::new());
    }
    let root_ids: Vec<i64> = roots.values().copied().collect();
    let muted_roots: HashSet<i64> = sqlx::query_scalar::<_, i64>(
        "SELECT root_status_id FROM conversation_mutes \
         WHERE account_id = $1 AND root_status_id = ANY($2)",
    )
    .bind(viewer)
    .bind(&root_ids)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    if muted_roots.is_empty() {
        return Ok(HashSet::new());
    }
    Ok(muted_ids(&roots, &muted_roots))
}

/// `POST /api/v1/statuses/{id}/mute` — idempotent; stores the thread
/// root, not the muted row itself.
pub async fn mute_conversation(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let root = roots_for(&ctx.db, &[status.id])
        .await?
        .get(&status.id)
        .copied()
        .ok_or_else(|| Error::NotFound("status".into()))?;
    sqlx::query(
        "INSERT INTO conversation_mutes (account_id, root_status_id) VALUES ($1,$2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(auth.account_id)
    .bind(root)
    .execute(&ctx.db)
    .await?;
    Ok(status)
}

/// `POST /api/v1/statuses/{id}/unmute` — idempotent; resolves the
/// same root the mute wrote (a mid-thread unmute still unmutes the
/// whole conversation, matching the mute).
pub async fn unmute_conversation(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
) -> Result<StatusRow> {
    let status = load_visible(ctx, status_id, Some(auth.account_id)).await?;
    let root = roots_for(&ctx.db, &[status.id])
        .await?
        .get(&status.id)
        .copied()
        .ok_or_else(|| Error::NotFound("status".into()))?;
    sqlx::query("DELETE FROM conversation_mutes WHERE account_id = $1 AND root_status_id = $2")
        .bind(auth.account_id)
        .bind(root)
        .execute(&ctx.db)
        .await?;
    Ok(status)
}
