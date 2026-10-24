//! Publish-site `EventBus` notifications for the timeline streams.
//!
//! Events go out where the write happens — the REST path
//! (`PostContext::bus`) for local posts, the pubby dispatch path
//! (`ApCore::bus`) for remote materialization. Both degrade to a
//! detached bus when streaming isn't reachable (worker process,
//! federation off), so producers never need to know which they got.
//!
//! Wire names follow `http::ws`: `public`, `public:local`,
//! `hashtag:{tag}`, `hashtag:local:{tag}`, scoped `user:{uid}` and
//! `list:{list_id}:{uid}` (`uid` is `users.id`, not `accounts.id` —
//! `Connection::matching` keys scoped delivery on it).

use sqlx::PgPool;

use crate::{
    activitypub::ApCore,
    http::ws::EventBus,
    jobs::JobContext,
    posts::{entity, PostContext, StatusRow},
};

/// A `PostContext` for the pubby dispatch path — `ApCore` carries the
/// same collaborators plus the bus. `ap` stays `None`: rendering and
/// streaming don't federate.
pub fn post_ctx_for_core(core: &ApCore) -> PostContext {
    let job = JobContext {
        db: core.db.clone(),
        redis: core.redis.clone(),
        config: core.config.clone(),
        media: crate::media::storage::MediaStore::from_config(&core.config),
    };
    let mut ctx = PostContext::from_job(&job, None);
    ctx.bus = core.bus.clone();
    ctx
}

/// Every stream name a status should reach, in one query set. The
/// status must be the *live* row shape (pre-tombstone) — deletes call
/// this before re-rendering.
async fn streams_for(db: &PgPool, status: &StatusRow) -> Vec<String> {
    // `direct` is a DM: it reaches only the mentioned local users' and
    // the author's own `user` streams — never public, hashtag or list
    // streams, and never a follower who wasn't addressed.
    if status.visibility == "direct" {
        return sqlx::query_scalar::<_, i64>(
            "SELECT u.id FROM users u \
             WHERE u.account_id = $2 \
                OR u.account_id IN (SELECT sm.account_id FROM status_mentions sm \
                                    WHERE sm.status_id = $1)",
        )
        .bind(status.id)
        .bind(status.account_id)
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|uid| format!("user:{uid}"))
        .collect();
    }
    let mut streams = Vec::new();
    if status.visibility == "public" {
        streams.push("public".to_string());
        if status.local {
            streams.push("public:local".to_string());
        }
        // Hashtag streams carry public posts only, like the REST tag
        // timeline.
        if let Ok(tags) = sqlx::query_scalar::<_, String>(
            "SELECT t.name FROM status_tags st \
             JOIN tags t ON t.id = st.tag_id WHERE st.status_id = $1",
        )
        .bind(status.id)
        .fetch_all(db)
        .await
        {
            for tag in tags {
                streams.push(format!("hashtag:{tag}"));
                if status.local {
                    streams.push(format!("hashtag:local:{tag}"));
                }
            }
        }
    }
    // Scoped `user` streams: the author's local followers plus the
    // author's own stream — home shows own posts, so the stream does
    // too (users.id, not accounts.id).
    if let Ok(user_ids) = sqlx::query_scalar::<_, i64>(
        "SELECT u.id FROM users u \
         WHERE u.account_id = $1 \
            OR u.account_id IN (SELECT f.account_id FROM follows f \
                                JOIN accounts a ON a.id = f.account_id AND a.domain IS NULL \
                                WHERE f.target_account_id = $1)",
    )
    .bind(status.account_id)
    .fetch_all(db)
    .await
    {
        streams.extend(user_ids.into_iter().map(|uid| format!("user:{uid}")));
    }
    // Scoped `list` streams: every local list containing the author.
    if let Ok(rows) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT l.id, u.id FROM lists l \
         JOIN list_accounts la ON la.list_id = l.id AND la.account_id = $1 \
         JOIN accounts a ON a.id = l.account_id AND a.domain IS NULL \
         JOIN users u ON u.account_id = l.account_id",
    )
    .bind(status.account_id)
    .fetch_all(db)
    .await
    {
        streams.extend(
            rows.into_iter()
                .map(|(list_id, uid)| format!("list:{list_id}:{uid}")),
        );
    }
    streams
}

/// `update` event — the status entity rendered unpersonalized
/// (`viewer: None`), matching what anonymous `public` subscribers can
/// see. Scoped subscribers render the same payload; Mastodon does the
/// same.
pub async fn publish_update(ctx: &PostContext, status: &StatusRow) {
    if status.tombstone {
        return;
    }
    let payload = match entity::status_entity(ctx, status, None).await {
        Ok(entity) => entity,
        Err(e) => {
            tracing::warn!(status_id = status.id, error = %e, "stream payload render failed");
            return;
        }
    };
    for stream in streams_for(&ctx.db, status).await {
        ctx.bus.publish(stream, "update", payload.clone());
    }
}

/// `delete` event — Mastodon's payload is the bare id string.
pub async fn publish_delete(db: &PgPool, bus: &EventBus, status: &StatusRow) {
    let payload = serde_json::json!(status.id.to_string());
    for stream in streams_for(db, status).await {
        bus.publish(stream, "delete", payload.clone());
    }
}
