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

use crate::{interactions::emit_notification_event, jobs::Dispatcher, posts::StatusRow, Result};

/// Emit `mention` and `status` notification events for a new status.
///
/// Mentions reach every mentioned local account (`status_mentions` is
/// the source of truth — `direct` posts still notify, which is how DMs
/// surface). `status` events reach local followers with
/// `follows.notify` and `subscriptions` rows — notify-without-following —
/// but only where the recipient could actually see the post: a `direct`
/// status never emits one (`mention` above is the DM surface — anything
/// more would leak the conversation to non-participants), and a
/// followers-only status reaches a subscriber only through an accepted
/// follow. `emit_notification_event` drops self/remote recipients
/// itself.
/// Lookup errors propagate — inside a job the caller must let them
/// retry (`?`), since a dropped lookup silently loses the
/// notification forever: the status is already durable, and dedupe
/// can only collapse events that were actually emitted.
pub async fn emit_for_status(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status: &StatusRow,
) -> Result<()> {
    let mentioned = sqlx::query_scalar::<_, i64>(
        "SELECT sm.account_id FROM status_mentions sm \
         JOIN accounts a ON a.id = sm.account_id AND a.domain IS NULL \
         WHERE sm.status_id = $1 AND sm.account_id <> $2",
    )
    .bind(status.id)
    .bind(status.account_id)
    .fetch_all(db)
    .await?;
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
    if status.visibility == "direct" {
        return Ok(());
    }
    let subscribed = sqlx::query_scalar::<_, i64>(
        "SELECT account_id FROM follows WHERE target_account_id = $1 AND notify \
         UNION \
         SELECT s.account_id FROM subscriptions s \
         WHERE s.target_account_id = $1 \
           AND ($2 IN ('public', 'unlisted') OR EXISTS ( \
               SELECT 1 FROM follows f \
               WHERE f.account_id = s.account_id AND f.target_account_id = $1))",
    )
    .bind(status.account_id)
    .bind(&status.visibility)
    .fetch_all(db)
    .await?;
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
    Ok(())
}
