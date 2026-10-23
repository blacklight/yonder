//! Status-publish emit points (epic 16): `mention` events for every
//! local mentioned account and `status` events for notify-flagged
//! followers/subscribers.
//!
//! Call [`emit_for_status`] once per status creation — the local
//! publish path (`posts::create::publish`) and the remote `Create`
//! materialization (`posts::remote::handle`) both funnel through it.
//! Edits never re-notify; the `ix_notifications_dedupe` index makes a
//! stray repeat harmless anyway.

use sqlx::PgPool;

use crate::{interactions::emit_notification_event, jobs::Dispatcher, posts::StatusRow};

/// Emit `mention` and `status` notification events for a new status.
///
/// Mentions reach every mentioned local account (`status_mentions` is
/// the source of truth — `direct` posts still notify, which is how DMs
/// surface). `status` events reach local followers with
/// `follows.notify` and `subscriptions` rows — notify-without-following.
/// `emit_notification_event` drops self/remote recipients itself.
pub async fn emit_for_status(db: &PgPool, dispatcher: &Dispatcher, status: &StatusRow) {
    let mentioned = match sqlx::query_scalar::<_, i64>(
        "SELECT sm.account_id FROM status_mentions sm \
         JOIN accounts a ON a.id = sm.account_id AND a.domain IS NULL \
         WHERE sm.status_id = $1 AND sm.account_id <> $2",
    )
    .bind(status.id)
    .bind(status.account_id)
    .fetch_all(db)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(status_id = status.id, error = %e, "mention lookup failed");
            Vec::new()
        }
    };
    for to in mentioned {
        emit_notification_event(
            db,
            dispatcher,
            "mention",
            status.account_id,
            Some(status.id),
            to,
        )
        .await;
    }
    let subscribed = match sqlx::query_scalar::<_, i64>(
        "SELECT account_id FROM follows WHERE target_account_id = $1 AND notify \
         UNION \
         SELECT account_id FROM subscriptions WHERE target_account_id = $1",
    )
    .bind(status.account_id)
    .fetch_all(db)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(status_id = status.id, error = %e, "status-notify lookup failed");
            Vec::new()
        }
    };
    for to in subscribed {
        emit_notification_event(
            db,
            dispatcher,
            "status",
            status.account_id,
            Some(status.id),
            to,
        )
        .await;
    }
}
