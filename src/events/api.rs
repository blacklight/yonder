//! Local RSVP API — backs `POST /api/v1/yonder/events/{id}/rsvp`,
//! `POST .../invitees` and `DELETE .../participants/{account_id}`.
//!
//! The HTTP layer normalizes the client's `{state}` into the same
//! [`rsvp::RsvpInput`] inbound AP activities use, so local joins,
//! leaves and invite responses share the §7.1 state machine. The
//! decide-and-apply transaction mirrors `inbound::decide_and_apply`:
//! lock the `events` row, re-read the participation, derive `invited`
//! from the locked row, transition, apply — counters can never race.

use serde::Deserialize;
use serde_json::json;

use crate::{
    events::{
        model::{EventParticipationRow, EventRow, ParticipationState},
        rsvp::{self, CounterDelta, OutboundActivity, RsvpOutcome},
    },
    posts::{self, PostContext},
    Error, Result,
};

/// `POST .../rsvp` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RsvpBody {
    /// `accepted` | `tentative` | `declined` | `none` (leave).
    pub state: String,
    pub message: Option<String>,
}

/// `POST .../invitees` body.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct InviteesBody {
    pub account_ids: Vec<i64>,
}

/// The caller's own participation row after an RSVP, for the response.
#[derive(Debug, serde::Serialize)]
pub struct RsvpResult {
    pub state: String,
    pub role: String,
}

/// Load the event status + row for `status_id`, enforcing that the
/// viewer can see it. `None` → the route 404s.
async fn load_visible_event(
    ctx: &PostContext,
    status_id: i64,
    viewer: i64,
) -> Result<Option<(posts::StatusRow, EventRow)>> {
    let Some(status) = posts::load(&ctx.db, status_id).await? else {
        return Ok(None);
    };
    if !posts::is_event(&status) {
        return Ok(None);
    }
    let visible = posts::visible_ids(&ctx.db, std::slice::from_ref(&status), Some(viewer)).await?;
    if !visible.contains(&status.id) {
        return Ok(None);
    }
    let Some(row) = sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE status_id = $1")
        .bind(status_id)
        .fetch_optional(&ctx.db)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((status, row)))
}

/// `(actor_id, username)` for a local account.
async fn actor_of(db: &sqlx::PgPool, account_id: i64) -> Result<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT actor_id, username FROM accounts WHERE id = $1 AND domain IS NULL",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))
}

async fn participation<'e, E>(
    db: E,
    status_id: i64,
    account_id: i64,
) -> Result<Option<EventParticipationRow>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_as::<_, EventParticipationRow>(
        "SELECT * FROM event_participations WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(account_id)
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// Organizer check: the status author or an `organizer`-role
/// participation row. Shared with the Nicolex adapter.
pub async fn is_organizer(
    db: &sqlx::PgPool,
    status: &posts::StatusRow,
    account_id: i64,
) -> Result<bool> {
    if status.account_id == account_id {
        return Ok(true);
    }
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM event_participations \
         WHERE status_id = $1 AND account_id = $2 AND role = 'organizer'",
    )
    .bind(status.id)
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    Ok(role.is_some())
}

/// RSVP environment for a local API call — mirrors the local-event
/// branch of `inbound`: no remote audience facts beyond the organizer
/// collection disclosed on the event.
async fn env_for(
    ctx: &PostContext,
    status: &posts::StatusRow,
    organizer_actor: &str,
) -> Result<rsvp::RsvpEnv> {
    let organizer_actors: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(a.actor_id, o.uri) FROM event_organizers o \
         LEFT JOIN accounts a ON a.id = o.account_id \
         WHERE o.status_id = $1",
    )
    .bind(status.id)
    .fetch_all(&ctx.db)
    .await?
    .into_iter()
    .flatten()
    .collect();
    Ok(rsvp::RsvpEnv {
        event_local: status.local,
        organizer_actor: organizer_actor.to_string(),
        attributed_to_actor: None,
        required_join_visibility: vec![],
        organizer_actors,
        our_actor: organizer_actor.to_string(),
        event_uri: status.uri.clone().unwrap_or_default(),
        now: chrono::Utc::now(),
    })
}

/// Map the API `{state}` + the caller's existing row onto the §7.1
/// input. `invited` rows resolve through the Friendica-style
/// Accept/TentativeAccept/Reject inputs; everyone else joins/leaves.
fn input_for(
    state: &str,
    existing: Option<&EventParticipationRow>,
    message: Option<String>,
) -> Result<rsvp::RsvpInput> {
    let invited = existing.is_some_and(|e| e.state == ParticipationState::Invited);
    match state {
        "none" => Ok(rsvp::RsvpInput::Leave),
        "accepted" if invited => Ok(rsvp::RsvpInput::AcceptInvite { activity_uri: None }),
        "tentative" if invited => Ok(rsvp::RsvpInput::TentativeAcceptInvite { activity_uri: None }),
        "declined" if invited => Ok(rsvp::RsvpInput::RejectInvite { activity_uri: None }),
        "accepted" => Ok(rsvp::RsvpInput::Join {
            join_uri: None,
            participation_message: message,
            tentative: false,
        }),
        "tentative" => Ok(rsvp::RsvpInput::Join {
            join_uri: None,
            participation_message: message,
            tentative: true,
        }),
        "declined" => {
            // Declining a pending/accepted remote join federates as
            // Leave; otherwise record the opt-out locally.
            if existing.is_some_and(|e| {
                matches!(
                    e.state,
                    ParticipationState::Pending
                        | ParticipationState::Accepted
                        | ParticipationState::Tentative
                )
            }) {
                Ok(rsvp::RsvpInput::Leave)
            } else {
                Ok(rsvp::RsvpInput::Decline)
            }
        }
        _ => Err(Error::Validation(
            "state must be accepted, tentative, declined or none".to_string(),
        )),
    }
}

/// Run the transition + apply in one transaction — the API-side twin
/// of `inbound::decide_and_apply`. `input` is built from the
/// participation row read *inside* the lock, so a concurrent
/// invite/decline can't flip the intended transition.
async fn decide_and_apply<F>(
    ctx: &PostContext,
    status_id: i64,
    account_id: i64,
    input: F,
    party: rsvp::RsvpParty,
    env: &rsvp::RsvpEnv,
) -> Result<Option<RsvpOutcome>>
where
    F: FnOnce(Option<&EventParticipationRow>) -> Result<rsvp::RsvpInput>,
{
    let mut tx = ctx.db.begin().await?;
    let event =
        sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE status_id = $1 FOR UPDATE")
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::NotFound("event".into()))?;
    let existing = participation(&mut *tx, status_id, account_id).await?;
    let mut party = party;
    party.invited = existing
        .as_ref()
        .is_some_and(|e| e.state == ParticipationState::Invited);
    let input = input(existing.as_ref())?;
    let Some(outcome) = rsvp::transition(&event, existing.as_ref(), input, &party, env)? else {
        return Ok(None);
    };
    rsvp::apply(&mut tx, status_id, account_id, &outcome).await?;
    tx.commit().await?;
    Ok(Some(outcome))
}

/// `POST /api/v1/yonder/events/{id}/rsvp`.
pub async fn rsvp(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
    body: &RsvpBody,
) -> Result<Option<RsvpResult>> {
    let (status, _row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    // Organizers don't RSVP to their own event.
    if status.account_id == account_id {
        return Err(Error::Validation("organizers cannot rsvp".to_string()));
    }
    let (actor_uri, username) = actor_of(&ctx.db, account_id).await?;
    let organizer_actor = actor_of(&ctx.db, status.account_id).await?.0;
    let state = body.state.clone();
    let message = body.message.clone();
    let party = rsvp::RsvpParty {
        account_id,
        actor_uri: actor_uri.clone(),
        username: Some(username.clone()),
        blocked: false,
        invited: false,
        authorized_responder: false,
    };
    let env = env_for(ctx, &status, &organizer_actor).await?;
    let Some(outcome) = decide_and_apply(
        ctx,
        status_id,
        account_id,
        move |existing| input_for(&state, existing, message.clone()),
        party,
        &env,
    )
    .await?
    else {
        // Transition dropped it (e.g. organizer Leave, blocked) —
        // report the unchanged row.
        return Ok(participation(&ctx.db, status_id, account_id)
            .await?
            .map(|p| RsvpResult {
                state: p.state.to_string(),
                role: p.role.to_string(),
            }));
    };
    crate::events::notify::emit_rsvp_notifications(
        &ctx.db,
        &ctx.dispatcher,
        &status,
        account_id,
        outcome.notify_organizer,
        outcome.notify_participant,
    )
    .await;
    // Outbound federation — only remote events need it; local events
    // update participants through the stream. For remote joins stamp
    // the Join's activity id as `request_activity_uri` (§7.2).
    if !status.local {
        if let (Some(ap), Some(outbound)) = (&ctx.ap, &outcome.outbound) {
            let doc = outbound.to_json(&actor_uri);
            let join_id = doc.get("id").and_then(|v| v.as_str()).map(str::to_string);
            match crate::activitypub::delivery::publish(ap, &username, &doc).await {
                Ok(_) => {
                    if matches!(outbound, OutboundActivity::Join { .. }) {
                        if let Some(id) = join_id {
                            if let Err(e) =
                                rsvp::stamp_request_uri(&ctx.db, status_id, account_id, &id).await
                            {
                                tracing::warn!(status_id, error = %e, "rsvp request uri stamp failed");
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(status_id, error = %e, "rsvp outbound publish failed"),
            }
        }
    }
    match participation(&ctx.db, status_id, account_id).await? {
        Some(p) => Ok(Some(RsvpResult {
            state: p.state.to_string(),
            role: p.role.to_string(),
        })),
        None => Ok(None),
    }
}

/// `POST /api/v1/yonder/events/{id}/invitees` — organizer-only;
/// records `invited` participation rows and (for direct events) the
/// mention rows that carry visibility + notifications.
pub async fn invitees(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
    body: &InviteesBody,
) -> Result<()> {
    let (status, _row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !is_organizer(&ctx.db, &status, account_id).await? {
        return Err(Error::Forbidden("only organizers can invite"));
    }
    let mut ids: Vec<i64> = body
        .account_ids
        .iter()
        .copied()
        .filter(|id| *id != account_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    // Only existing local accounts.
    let found: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE id = ANY($1) AND domain IS NULL")
            .bind(&ids)
            .fetch_all(&ctx.db)
            .await?;
    if found.len() != ids.len() {
        return Err(Error::Validation(
            "invitees must be existing local accounts".to_string(),
        ));
    }
    let mut tx = ctx.db.begin().await?;
    // Rows already decided (accepted/tentative/declined/pending) keep
    // their state — re-inviting must not resurrect or re-notify them.
    // `xmax = 0` on RETURNING marks freshly inserted rows.
    let mut newly_invited = Vec::new();
    for id in &ids {
        let inserted: Option<bool> = sqlx::query_scalar(
            "INSERT INTO event_participations (status_id, account_id, state, protocol) \
             VALUES ($1, $2, 'invited', 'local') \
             ON CONFLICT (status_id, account_id) DO UPDATE SET \
                 state = 'invited', updated_at = now() \
             WHERE event_participations.state = 'invited' \
               AND event_participations.role <> 'organizer' \
             RETURNING (xmax = 0)",
        )
        .bind(status_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted == Some(true) {
            newly_invited.push(*id);
        }
        if status.visibility == "direct" {
            sqlx::query(
                "INSERT INTO status_mentions (status_id, account_id) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(status_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
    }
    // Merge into the authoring invitee list (union, deduped).
    sqlx::query(
        "UPDATE events SET invitees = \
             (SELECT array_agg(DISTINCT x ORDER BY x) FROM unnest(invitees || $2::bigint[]) x), \
             updated_at = now() \
         WHERE status_id = $1",
    )
    .bind(status_id)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // `pleroma:participation_accepted` tells each *newly* invited
    // account they're on the list — decided rows are untouched above.
    for id in newly_invited {
        crate::interactions::emit_notification_event(
            &ctx.db,
            &ctx.dispatcher,
            "pleroma:participation_accepted",
            account_id,
            Some(status_id),
            id,
        )
        .await;
    }
    Ok(())
}

/// `DELETE /api/v1/yonder/events/{id}/participants/{account_id}` —
/// organizer removes a participant: pending joins get a `Reject`
/// (with the stored join uri), accepted ones a revocation `Reject`
/// (Mobilizon semantics); the row is deleted either way.
pub async fn remove_participant(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
    target_account_id: i64,
) -> Result<()> {
    let (status, _row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !is_organizer(&ctx.db, &status, account_id).await? {
        return Err(Error::Forbidden("only organizers can remove participants"));
    }
    // Delete + counter fix in one tx — the participation row and the
    // events counters are re-read under the event lock so a concurrent
    // RSVP can't skew the delta.
    let mut tx = ctx.db.begin().await?;
    sqlx::query("SELECT status_id FROM events WHERE status_id = $1 FOR UPDATE")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    let Some(existing) = participation(&mut *tx, status_id, target_account_id).await? else {
        return Err(Error::NotFound("participation".into()));
    };
    if existing.is_organizer() {
        return Err(Error::Validation(
            "organizer participations cannot be removed".to_string(),
        ));
    }
    // Remote participants get a Reject answering their stored join —
    // pending *and* accepted rows (revocation), per the contract.
    let mut outbound = None;
    if !status.local {
        if let Some(target_actor) = sqlx::query_scalar::<_, String>(
            "SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NOT NULL",
        )
        .bind(target_account_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let object = existing
                .request_activity_uri
                .clone()
                .unwrap_or_else(|| status.uri.clone().unwrap_or_default());
            outbound = Some(OutboundActivity::Reject {
                object,
                to: vec![target_actor],
                cc: vec![],
            });
        }
    }
    sqlx::query("DELETE FROM event_participations WHERE status_id = $1 AND account_id = $2")
        .bind(status_id)
        .bind(target_account_id)
        .execute(&mut *tx)
        .await?;
    let delta = CounterDelta {
        accepted: -i32::from(existing.state == ParticipationState::Accepted),
        pending: -i32::from(existing.state == ParticipationState::Pending),
    };
    if delta.accepted != 0 || delta.pending != 0 {
        sqlx::query(
            "UPDATE events SET participants_count = GREATEST(participants_count + $2, 0), \
                 pending_count = GREATEST(pending_count + $3, 0), updated_at = now() \
             WHERE status_id = $1",
        )
        .bind(status_id)
        .bind(delta.accepted)
        .bind(delta.pending)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let (Some(ap), Some(outbound)) = (&ctx.ap, outbound) {
        let (organizer_actor, organizer_username) = actor_of(&ctx.db, status.account_id).await?;
        let doc = outbound.to_json(&organizer_actor);
        if let Err(e) = crate::activitypub::delivery::publish(ap, &organizer_username, &doc).await {
            tracing::warn!(status_id, error = %e, "participant reject publish failed");
        }
    }
    // Also drop any invitee mention/authoring-list entry.
    sqlx::query("UPDATE events SET invitees = array_remove(invitees, $2) WHERE status_id = $1")
        .bind(status_id)
        .bind(target_account_id)
        .execute(&ctx.db)
        .await?;
    if status.visibility == "direct" {
        sqlx::query("DELETE FROM status_mentions WHERE status_id = $1 AND account_id = $2")
            .bind(status_id)
            .bind(target_account_id)
            .execute(&ctx.db)
            .await?;
    }
    Ok(())
}

/// Organizer decision on a pending join — `authorize` accepts,
/// `reject` declines. Sends the matching response to remote
/// joiners; local pending joins resolve silently.
pub async fn decide_request(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
    target_account_id: i64,
    approve: bool,
) -> Result<()> {
    let (status, _row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !is_organizer(&ctx.db, &status, account_id).await? {
        return Err(Error::Forbidden("only organizers can decide requests"));
    }
    // Lock the event row, re-read the participation and apply the
    // decision atomically — a double click or concurrent call finds
    // the row no longer `pending` and gets a conflict instead of
    // double-decrementing the counters.
    let new_state = if approve {
        ParticipationState::Accepted
    } else {
        ParticipationState::Rejected
    };
    let mut tx = ctx.db.begin().await?;
    sqlx::query("SELECT status_id FROM events WHERE status_id = $1 FOR UPDATE")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    let existing = participation(&mut *tx, status_id, target_account_id)
        .await?
        .ok_or_else(|| Error::NotFound("participation".into()))?;
    if existing.state != ParticipationState::Pending {
        return Err(Error::Conflict(
            "only pending joins can be decided".to_string(),
        ));
    }
    let updated = sqlx::query(
        "UPDATE event_participations SET state = $3, updated_at = now() \
         WHERE status_id = $1 AND account_id = $2 AND state = 'pending'",
    )
    .bind(status_id)
    .bind(target_account_id)
    .bind(new_state.to_string())
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(Error::Conflict(
            "only pending joins can be decided".to_string(),
        ));
    }
    // pending → accepted counts a participant; pending → rejected
    // only clears the pending slot.
    let delta = CounterDelta {
        accepted: i32::from(approve),
        pending: -1,
    };
    sqlx::query(
        "UPDATE events SET participants_count = GREATEST(participants_count + $2, 0), \
             pending_count = GREATEST(pending_count + $3, 0), updated_at = now() \
         WHERE status_id = $1",
    )
    .bind(status_id)
    .bind(delta.accepted)
    .bind(delta.pending)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // Approvals reach the participant as `pleroma:participation_accepted`;
    // rejections send no notification (the contract defines none).
    if approve {
        crate::interactions::emit_notification_event(
            &ctx.db,
            &ctx.dispatcher,
            "pleroma:participation_accepted",
            account_id,
            Some(status_id),
            target_account_id,
        )
        .await;
    }
    // Remote joiners get the Accept/Reject answering their Join.
    if !status.local {
        if let Some(target_actor) = sqlx::query_scalar::<_, String>(
            "SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NOT NULL",
        )
        .bind(target_account_id)
        .fetch_optional(&ctx.db)
        .await?
        {
            if let Some(ap) = &ctx.ap {
                let (organizer_actor, organizer_username) =
                    actor_of(&ctx.db, status.account_id).await?;
                let object = existing
                    .request_activity_uri
                    .clone()
                    .unwrap_or_else(|| status.uri.clone().unwrap_or_default());
                let outbound = if approve {
                    OutboundActivity::Accept {
                        object,
                        to: vec![target_actor],
                        cc: vec![],
                    }
                } else {
                    OutboundActivity::Reject {
                        object,
                        to: vec![target_actor],
                        cc: vec![],
                    }
                };
                let doc = outbound.to_json(&organizer_actor);
                if let Err(e) =
                    crate::activitypub::delivery::publish(ap, &organizer_username, &doc).await
                {
                    tracing::warn!(status_id, error = %e, "rsvp decision publish failed");
                }
            }
        }
    }
    Ok(())
}

/// `GET .../participations`-style listing for the organizer UI —
/// pending rows with their message, accepted/tentative for review.
pub async fn list_participations(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
) -> Result<Vec<serde_json::Value>> {
    let (status, _row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !is_organizer(&ctx.db, &status, account_id).await? {
        return Err(Error::Forbidden("only organizers can list participations"));
    }
    let rows = sqlx::query_as::<_, (i64, String, Option<String>, Option<String>)>(
        "SELECT p.account_id, p.state, p.participation_message, a.username \
         FROM event_participations p JOIN accounts a ON a.id = p.account_id \
         WHERE p.status_id = $1 AND p.role <> 'organizer' \
         ORDER BY p.created_at",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, state, message, username)| {
            json!({
                "account_id": id.to_string(),
                "username": username,
                "state": state,
                "participation_message": message,
            })
        })
        .collect())
}
