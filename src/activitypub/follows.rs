//! Follow-handshake wiring — `on_follow_response`, the accept/reject
//! helpers and the remote→local `Accept`/`Reject` settlement.
//!
//! When we send a `Follow` to a remote actor, pubby records a pending
//! `follow_requests` row (local actor → remote target). Their
//! `Accept`/`Reject` answer is handled natively by pubby's inbox: it
//! removes the pending row and fires `on_follow_response(target,
//! accepted)`. Our callback writes the `follows` row on accept — the
//! direction is *local follower → remote target*, the mirror image of
//! the inbound `store_follower` write.
//!
//! The local follower is implicit in the callback (pubby only passes
//! the target URL), so [`on_follow_response_fn`] binds the *handling
//! actor's* id at handler-build time — the callback fires on the
//! resolved per-user handler, which is the account that sent the
//! original `Follow`.

use std::sync::Arc;

use pubby_sdk::handlers::inbox::OnFollowResponse;
use sqlx::PgPool;

use crate::Result;

use super::actors::sync_block;
use super::ApCore;

/// Bound per handler — `local_actor_id` is the account that sent the
/// original `Follow`. On `accepted` insert the `follows` row; on reject
/// the pending request is already gone — just audit.
pub fn on_follow_response_fn(
    core: &Arc<ApCore>,
    local_actor_id: impl Into<String>,
) -> OnFollowResponse {
    let core = core.clone();
    let local_actor_id = local_actor_id.into();
    Arc::new(move |target_actor_id: &str, accepted: bool| {
        let core = core.clone();
        let local_actor_id = local_actor_id.clone();
        let target_actor_id = target_actor_id.to_string();
        let outcome = sync_block(async {
            if accepted {
                accept_outbound(&core.db, &local_actor_id, &target_actor_id).await
            } else {
                tracing::info!(
                    follower = %local_actor_id,
                    target = %target_actor_id,
                    "outbound follow rejected"
                );
                Ok(())
            }
        });
        if let Err(e) = outcome {
            tracing::error!(
                error = %e,
                target = %target_actor_id,
                "on_follow_response bookkeeping failed"
            );
        }
    })
}

/// Insert the `local → remote` follows row that completes an accepted
/// outbound follow.
async fn accept_outbound(pool: &PgPool, local_actor_id: &str, target_actor_id: &str) -> Result<()> {
    // Make sure the remote target exists as an accounts row — the
    // follow target URL is all we may have at this point.
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1, $2, $3, $3, '') ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(super::moderation::actor_domain(target_actor_id))
    .bind(
        target_actor_id
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(target_actor_id),
    )
    .bind(target_actor_id)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO follows (account_id, target_account_id)
        SELECT la.id, ra.id
        FROM accounts la, accounts ra
        WHERE la.actor_id = $1 AND la.domain IS NULL AND ra.actor_id = $2
        ON CONFLICT (account_id, target_account_id) DO NOTHING
        "#,
    )
    .bind(local_actor_id)
    .bind(target_actor_id)
    .execute(pool)
    .await?;
    // Counters: the `follows` triggers (migration `0016`) own them.
    Ok(())
}
