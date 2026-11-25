//! Event notification emissions (Section 12) — thin wrappers over
//! `interactions::emit_notification_event`, which enqueues the
//! `NotificationFanout` job. Fan-out suppression (blocks, mutes,
//! domain blocks, non-local recipients) is re-checked at run time.
//!
//! Types:
//! - `pleroma:participation_request` — organizer(s), pending join on
//!   a restricted/invite event.
//! - `pleroma:participation_accepted` — the participant, on invite
//!   responses and organizer authorize/reject decisions.
//! - `pleroma:event_update` — local participants on event changes
//!   (edit, remote sync, pre-tombstone delete).
//! - `pleroma:event_reminder` — the reminder sweep only.

use sqlx::PgPool;

use crate::{interactions::emit_notification_event, jobs::Dispatcher, posts::StatusRow, Result};

/// RSVP-transition side effects: `notify_organizer` pings every local
/// organizer account; `notify_participant` pings the actor whose
/// participation just changed. `actor_account_id` is the account the
/// transition belongs to (remote actors resolve to their local stub —
/// `emit_notification_event` drops non-local *recipients*, and a
/// remote organizer simply never receives).
pub async fn emit_rsvp_notifications(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status: &StatusRow,
    actor_account_id: i64,
    notify_organizer: bool,
    notify_participant: bool,
) {
    if notify_organizer {
        // Local organizer accounts only — remote co-organizers can't
        // receive Yonder notifications.
        let organizers: Vec<i64> = match sqlx::query_scalar::<_, i64>(
            "SELECT p.account_id FROM event_participations p \
             JOIN accounts a ON a.id = p.account_id AND a.domain IS NULL \
             WHERE p.status_id = $1 AND p.role = 'organizer'",
        )
        .bind(status.id)
        .fetch_all(db)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(status_id = status.id, error = %e, "organizer lookup failed");
                Vec::new()
            }
        };
        for to in organizers {
            emit_notification_event(
                db,
                dispatcher,
                "pleroma:participation_request",
                actor_account_id,
                Some(status.id),
                to,
            )
            .await;
        }
    }
    if notify_participant {
        emit_notification_event(
            db,
            dispatcher,
            "pleroma:participation_accepted",
            status.account_id,
            Some(status.id),
            actor_account_id,
        )
        .await;
    }
}

/// `pleroma:event_update` to every local participant holding an
/// active-ish row (`accepted`, `tentative`, `pending`, `invited`) —
/// declined/rejected/ignored/withdrawn rows stay silent. The status
/// author is excluded (they made the change); organizer rows are
/// excluded too — the author *is* the organizer on local events.
pub async fn emit_event_update(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status: &StatusRow,
) -> Result<()> {
    let participants: Vec<i64> = sqlx::query_scalar(
        "SELECT p.account_id FROM event_participations p \
         JOIN accounts a ON a.id = p.account_id AND a.domain IS NULL \
         WHERE p.status_id = $1 \
           AND p.state IN ('accepted','tentative','pending','invited') \
           AND p.account_id <> $2",
    )
    .bind(status.id)
    .bind(status.account_id)
    .fetch_all(db)
    .await?;
    for to in participants {
        // Owner decision 3: participants get the standard Mastodon
        // `update` notification *and* the event-specific
        // `pleroma:event_update` — different types, so the dedupe
        // index keeps both rows.
        for kind in ["update", "pleroma:event_update"] {
            emit_notification_event(db, dispatcher, kind, status.account_id, Some(status.id), to)
                .await;
        }
    }
    Ok(())
}

/// `pleroma:event_update` to a specific set of accounts — the remote
/// `Invite` path knows exactly who was just invited and must not fan
/// out to participants who were already on the event.
pub async fn emit_event_update_to(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status: &StatusRow,
    account_ids: &[i64],
) {
    for &to in account_ids {
        emit_notification_event(
            db,
            dispatcher,
            "pleroma:event_update",
            status.account_id,
            Some(status.id),
            to,
        )
        .await;
    }
}

/// `pleroma:event_reminder` for one participation row — called by the
/// `EventReminderSweep` job once per (participation, start_time).
/// Fallible so the sweep only stamps `reminded_at` after a successful
/// enqueue (a failed enqueue retries on the next sweep tick).
pub async fn emit_event_reminder(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status: &StatusRow,
    participant_account_id: i64,
) -> Result<()> {
    crate::interactions::emit_notification_event_ext(
        db,
        dispatcher,
        "pleroma:event_reminder",
        status.account_id,
        Some(status.id),
        None,
        participant_account_id,
    )
    .await
}

/// Convenience for call sites holding only ids — loads the status row
/// then delegates. Missing/tombstoned rows are a no-op.
pub async fn emit_event_update_by_id(
    db: &PgPool,
    dispatcher: &Dispatcher,
    status_id: i64,
) -> Result<()> {
    let Some(status) = crate::posts::load(db, status_id).await? else {
        return Ok(());
    };
    emit_event_update(db, dispatcher, &status).await
}

/// Stamp `reminded_at` — idempotency marker for the sweep. Called
/// after the notification enqueue succeeds.
pub async fn mark_reminded(db: &PgPool, status_id: i64, account_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE event_participations SET reminded_at = now() \
         WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(account_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Re-arm reminders after a reschedule — clears `reminded_at` on all
/// participant rows so the sweep fires again for the new start.
pub async fn rearm_reminders(db: &PgPool, status_id: i64) -> Result<()> {
    sqlx::query("UPDATE event_participations SET reminded_at = NULL WHERE status_id = $1")
        .bind(status_id)
        .execute(db)
        .await?;
    Ok(())
}
