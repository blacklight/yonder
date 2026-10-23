//! Status-entity contributions for interactions — viewer-relative
//! flags, the quote block and the Pleroma `emoji_reactions` list.
//! Consumed by `posts::entity::status_entity` (reblog nesting lives
//! there, one recursion level).

use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    posts::{self, PostContext, StatusRow},
    Result,
};

/// The viewer-relative flags `reblogged`/`favourited`/`bookmarked`
/// plus the author-relative `pinned`. Anonymous viewers get all
/// `false` (except `pinned`, which is author state).
pub async fn viewer_flags(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
) -> Result<Value> {
    let pinned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM status_pins WHERE account_id = $1 AND status_id = $2)",
    )
    .bind(status.account_id)
    .bind(status.id)
    .fetch_one(&ctx.db)
    .await?;
    let Some(viewer) = viewer else {
        return Ok(json!({
            "reblogged": false,
            "favourited": false,
            "bookmarked": false,
            "pinned": pinned,
        }));
    };
    let row = sqlx::query(
        "SELECT \
           EXISTS(SELECT 1 FROM favourites WHERE account_id = $2 AND status_id = $1) AS faved, \
           EXISTS(SELECT 1 FROM bookmarks WHERE account_id = $2 AND status_id = $1) AS marked, \
           EXISTS(SELECT 1 FROM statuses WHERE account_id = $2 AND reblog_of_id = $1 \
                  AND NOT tombstone) AS boosted",
    )
    .bind(status.id)
    .bind(viewer)
    .fetch_one(&ctx.db)
    .await?;
    Ok(json!({
        "reblogged": row.get::<bool, _>("boosted"),
        "favourited": row.get::<bool, _>("faved"),
        "bookmarked": row.get::<bool, _>("marked"),
        "pinned": pinned,
    }))
}

/// The Mastodon 4.4 quote block: `quote_id`, the `quote` object
/// (`state` + `quoted_status` when the viewer can see it) and the
/// `yonder` extension carrying the quote uri. `Null` for non-quotes.
/// `depth` bounds nesting: past one level `quoted_status` renders
/// `null` — quote cycles (remote-constructible via the pending-quote
/// backfill) would otherwise recurse without bound.
pub async fn quote_json(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
    depth: u8,
) -> Result<Value> {
    let (id, uri, quoted_status, state) = match (status.quote_of_id, &status.quote_uri) {
        (Some(qid), _) => {
            let quoted = posts::load(&ctx.db, qid).await?;
            let quoted_status = match &quoted {
                Some(q) if depth == 0 && posts::can_see(&ctx.db, q, viewer).await? => {
                    // Boxed: status_entity → quote_json → status_entity
                    // is an async recursion cycle (bounded by depth).
                    Some(
                        Box::pin(posts::entity::status_entity_inner(
                            ctx,
                            q,
                            viewer,
                            depth + 1,
                        ))
                        .await?,
                    )
                }
                _ => None,
            };
            (json!(qid.to_string()), None, quoted_status, "accepted")
        }
        (None, Some(uri)) => (Value::Null, Some(uri.clone()), None, "pending"),
        (None, None) => return Ok(Value::Null),
    };
    Ok(json!({
        "quote_id": id,
        "quote": {
            "state": state,
            "quoted_status": quoted_status,
        },
        "yonder_quote_uri": uri,
    }))
}
