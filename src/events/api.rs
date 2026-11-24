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
    http::api::v1::EntityId,
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
    /// Account ids (local or remote) — string or number; snowflake
    /// values exceed JS's `Number.MAX_SAFE_INTEGER`, so clients send
    /// strings.
    pub account_ids: Vec<EntityId>,
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

/// The actor URI of any account, local or remote — the event
/// organizer on a remote event is a remote account, so audience
/// resolution must not go through the local-only [`actor_of`].
async fn actor_uri_of(db: &sqlx::PgPool, account_id: i64) -> Result<String> {
    sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
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
    our_actor: &str,
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
        our_actor: our_actor.to_string(),
        event_uri: status.uri.clone().unwrap_or_default(),
        now: chrono::Utc::now(),
    })
}

/// Map the API `{state}` + the caller's existing row onto the §7.1
/// input. `invited` rows resolve through the Friendica-style
/// Accept/TentativeAccept/Reject inputs — except on remote events
/// whose `remote_join_protocol` is `join` (Mobilizon/Catodon/FEP),
/// where the answer is a `Join`: DM invites carry no Invite activity
/// to respond to, and `Join` is the dialect those servers understand.
fn input_for(
    state: &str,
    existing: Option<&EventParticipationRow>,
    event: &EventRow,
    message: Option<String>,
) -> Result<rsvp::RsvpInput> {
    let invited = existing.is_some_and(|e| e.state == ParticipationState::Invited);
    let join_protocol =
        event.remote_join_protocol == Some(crate::events::model::RemoteJoinProtocol::Join);
    match state {
        "none" => Ok(rsvp::RsvpInput::Leave),
        "accepted" if invited && join_protocol => Ok(rsvp::RsvpInput::Join {
            join_uri: None,
            participation_message: message,
            tentative: false,
        }),
        "tentative" if invited && join_protocol => Ok(rsvp::RsvpInput::Join {
            join_uri: None,
            participation_message: message,
            tentative: true,
        }),
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
    F: FnOnce(Option<&EventParticipationRow>, &EventRow) -> Result<rsvp::RsvpInput>,
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
    let input = input(existing.as_ref(), &event)?;
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
    // The organizer may be a remote account (RSVP on a remote event) —
    // a local-only lookup here 404'd every remote-event RSVP.
    let organizer_actor = actor_uri_of(&ctx.db, status.account_id).await?;
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
    let env = env_for(ctx, &status, &organizer_actor, &actor_uri).await?;
    let Some(outcome) = decide_and_apply(
        ctx,
        status_id,
        account_id,
        move |existing, event| input_for(&state, existing, event, message.clone()),
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
        .map(EntityId::to_i64)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|id| *id != account_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(());
    }
    // Same rule as authoring validation: invitees are a
    // `direct`-visibility mechanism (mentions carry the audience).
    if status.visibility != "direct" {
        return Err(Error::Validation(
            "event.invitees requires visibility=direct".to_string(),
        ));
    }
    // Existing accounts — local or remote stubs (§16 invites remote
    // actors through `Invite` activities, delivered post-commit).
    let found: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, domain FROM accounts WHERE id = ANY($1) AND deleted_at IS NULL")
            .bind(&ids)
            .fetch_all(&ctx.db)
            .await?;
    if found.len() != ids.len() {
        return Err(Error::Validation(
            "invitees must be existing accounts".to_string(),
        ));
    }
    let remote_ids: Vec<i64> = found
        .iter()
        .filter(|(_, domain)| domain.is_some())
        .map(|(id, _)| *id)
        .collect();
    let mut tx = ctx.db.begin().await?;
    // Rows already decided (accepted/tentative/declined/pending) keep
    // their state — re-inviting must not resurrect or re-notify them.
    // `xmax = 0` on RETURNING marks freshly inserted rows.
    let mut newly_invited = Vec::new();
    for id in &ids {
        // Remote invitees answer over ActivityPub — `invite_response`;
        // local ones use the API (`local`).
        let protocol = if remote_ids.contains(id) {
            "invite_response"
        } else {
            "local"
        };
        let inserted: Option<bool> = sqlx::query_scalar(
            "INSERT INTO event_participations (status_id, account_id, state, protocol) \
             VALUES ($1, $2, 'invited', $3) \
             ON CONFLICT (status_id, account_id) DO UPDATE SET \
                 state = 'invited', updated_at = now() \
             WHERE event_participations.state = 'invited' \
               AND event_participations.role <> 'organizer' \
             RETURNING (xmax = 0)",
        )
        .bind(status_id)
        .bind(id)
        .bind(protocol)
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
    // Remote invitees get the ActivityPub `Invite` — the only signal
    // their instance sees (§16). The state filter keeps decided rows
    // (declined etc.) silent: re-inviting must not resurrect them.
    if !remote_ids.is_empty() {
        let invited_remote: Vec<i64> = sqlx::query_scalar(
            "SELECT account_id FROM event_participations \
             WHERE status_id = $1 AND state = 'invited' AND account_id = ANY($2)",
        )
        .bind(status_id)
        .bind(&remote_ids)
        .fetch_all(&ctx.db)
        .await?;
        publish_invites(ctx, &status, account_id, &invited_remote).await?;
    }
    Ok(())
}

/// `POST /api/v1/yonder/events/{id}/dm-invites` — organizer-only;
/// Catodon-style invitations for events of *any* visibility. Sends
/// one `direct` status mentioning every invitee, marked with
/// `event_invite_uri` so the AP document carries
/// `yonder:inviteTo`/`catodon:inviteTo`. Unlike [`invitees`] this
/// grants no visibility and writes no participation rows — it is a
/// notification, and the event's own RSVP protocol applies.
pub async fn dm_invites(
    ctx: &PostContext,
    auth: &crate::http::middleware::auth::AuthContext,
    account_id: i64,
    status_id: i64,
    body: &InviteesBody,
) -> Result<serde_json::Value> {
    let (status, row) = load_visible_event(ctx, status_id, account_id)
        .await?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !is_organizer(&ctx.db, &status, account_id).await? {
        return Err(Error::Forbidden("only organizers can invite"));
    }
    let mut ids: Vec<i64> = body
        .account_ids
        .iter()
        .map(EntityId::to_i64)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|id| *id != account_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Err(Error::Validation("account_ids must not be empty".into()));
    }
    let invitees: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, username, domain FROM accounts \
         WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY id",
    )
    .bind(&ids)
    .fetch_all(&ctx.db)
    .await?;
    if invitees.len() != ids.len() {
        return Err(Error::Validation(
            "invitees must be existing accounts".to_string(),
        ));
    }
    let event_uri = status
        .uri
        .clone()
        .ok_or_else(|| Error::Validation("event has no AP uri".into()))?;
    let text = dm_invite_text(ctx, &status, &row, status_id, &invitees).await?;
    let params = posts::create::StatusParams {
        status: text,
        content_type: None,
        visibility: Some("direct".to_string()),
        language: None,
        sensitive: Some(false),
        spoiler_text: None,
        in_reply_to_id: None,
        media_ids: None,
        poll: None,
        quote_id: None,
        scheduled_at: None,
        post_type: Some("note".to_string()),
        title: None,
        summary: None,
        cover_media_id: None,
        event: None,
        event_invite_uri: Some(event_uri),
    };
    match posts::create::create_status(ctx, auth, &params, None).await? {
        posts::create::CreateOutcome::Published(entity)
        | posts::create::CreateOutcome::Scheduled(entity) => Ok(entity),
    }
}

/// The DM body — event name, when/where summary and the event link,
/// followed by the mention audience that makes it a `direct` post.
async fn dm_invite_text(
    ctx: &PostContext,
    status: &posts::StatusRow,
    row: &EventRow,
    status_id: i64,
    invitees: &[(i64, String, Option<String>)],
) -> Result<String> {
    let title = status
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Event".to_string());
    let mut lines = vec![title];
    if let Some(when) = dm_when(row) {
        lines.push(when);
    }
    if let Some(where_) = dm_where(ctx, status_id).await? {
        lines.push(where_);
    }
    if let Some(link) = status.url.clone().or_else(|| status.uri.clone()) {
        lines.push(link);
    }
    let mentions: Vec<String> = invitees
        .iter()
        .map(|(_, username, domain)| match domain {
            Some(d) => format!("@{username}@{d}"),
            None => format!("@{username}"),
        })
        .collect();
    let mut text = lines.join("\n\n");
    text.push_str("\n\n");
    text.push_str(&mentions.join(" "));
    Ok(text)
}

/// `<date> <time> – <date> <time> (<zone>)` in the event's timezone.
fn dm_when(row: &EventRow) -> Option<String> {
    let start = row.start_time?;
    let tz = row
        .timezone
        .as_deref()
        .and_then(|t| t.parse::<chrono_tz::Tz>().ok());
    let fmt = |dt: chrono::DateTime<chrono::Utc>| match tz {
        Some(tz) => dt
            .with_timezone(&tz)
            .format("%b %-d, %Y %-I:%M %p")
            .to_string(),
        None => dt.format("%b %-d, %Y %-I:%M %p").to_string(),
    };
    let mut out = fmt(start);
    if let Some(end) = row.end_time {
        out.push_str(" – ");
        out.push_str(&fmt(end));
    }
    if let Some(tz) = row.timezone.as_deref() {
        out.push_str(&format!(" ({tz})"));
    }
    Some(out)
}

/// First location's display text — name, free-text address, or the
/// structured parts joined.
async fn dm_where(ctx: &PostContext, status_id: i64) -> Result<Option<String>> {
    let bundles = crate::events::entity::load_many(&ctx.db, &[status_id], None).await?;
    let Some(bundle) = bundles.get(&status_id) else {
        return Ok(None);
    };
    let Some(loc) = bundle.locations.first() else {
        return Ok(None);
    };
    let text = loc
        .name
        .clone()
        .or_else(|| loc.address_text.clone())
        .or_else(|| {
            let joined = [
                loc.street_address.as_deref(),
                loc.locality.as_deref(),
                loc.region.as_deref(),
                loc.postal_code.as_deref(),
                loc.country.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
            (!joined.is_empty()).then_some(joined)
        })
        .filter(|s: &String| !s.is_empty());
    Ok(text)
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
    // Remote participants get a Reject answering their stored join
    // or invite — pending *and* accepted rows (revocation), and
    // `invited` rows on local events too (§16 remote invitees). Local
    // events only: the Reject is published as the event author, which
    // a remote event's author can never be.
    let mut outbound = None;
    if status.local {
        if let Some(target_actor) = sqlx::query_scalar::<_, String>(
            "SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NOT NULL",
        )
        .bind(target_account_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let object = existing
                .invite_activity_uri
                .clone()
                .or_else(|| existing.request_activity_uri.clone())
                .or_else(|| status.uri.clone())
                .unwrap_or_default();
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
    // Remote joiners get the Accept/Reject answering their Join. Only
    // meaningful on local events: the response is published as the
    // event author, which requires the author to be a local account.
    if status.local {
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
    let rows = sqlx::query_as::<_, (i64, String, Option<String>)>(
        "SELECT p.account_id, p.state, p.participation_message \
         FROM event_participations p \
         WHERE p.status_id = $1 AND p.role <> 'organizer' \
         ORDER BY p.created_at",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    // Embed the full Account entity (avatar, display name, acct,
    // emojis) so the organizer panel can render curated rows.
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let accounts = crate::users::account::load_many_incl_suspended(&ctx.db, &ids).await?;
    let entities =
        crate::users::account::account_entities(&ctx.db, &ctx.config, &ctx.base_url(), &accounts)
            .await?;
    let entities: std::collections::HashMap<i64, serde_json::Value> =
        accounts.iter().map(|a| a.id).zip(entities).collect();
    Ok(rows
        .into_iter()
        .map(|(id, state, message)| {
            json!({
                "account_id": id.to_string(),
                "account": entities.get(&id).cloned().unwrap_or(serde_json::Value::Null),
                "state": state,
                "participation_message": message,
            })
        })
        .collect())
}

/// §16 invitees-only fan-out — one `Invite{object: event_uri,
/// to: [invitee]}` per invitee, published as the organizer. Also
/// records the `invited` participation rows (the Events page "invited"
/// tab and the §7.1 invite-response transitions read them). Local
/// invitees already get the mention notification through
/// `status_mentions`; the `Invite` is the ActivityPub-compatible
/// signal (and the only one remote invitees see).
pub async fn create_invited_rows_and_invite(
    ctx: &PostContext,
    status: &posts::StatusRow,
    organizer_account_id: i64,
    invitee_ids: &[i64],
) -> Result<()> {
    if invitee_ids.is_empty() {
        return Ok(());
    }
    // `invited` participation rows — an existing row is left untouched
    // (`DO NOTHING`), so decided states are never overwritten.
    for invitee in invitee_ids {
        sqlx::query(
            "INSERT INTO event_participations \
                 (status_id, account_id, role, state, protocol) \
             VALUES ($1, $2, 'participant', 'invited', 'invite_response') \
             ON CONFLICT (status_id, account_id) DO NOTHING",
        )
        .bind(status.id)
        .bind(invitee)
        .execute(&ctx.db)
        .await?;
    }
    publish_invites(ctx, status, organizer_account_id, invitee_ids).await
}

/// Publish one `Invite{object: event_uri, to: [invitee actor]}` per
/// invitee as the organizer, stamping the activity id onto the
/// invitee's `invite_activity_uri` so a Friendica-style
/// `Accept{object: invite_id}` resolves back to the event (§16).
/// Publish failures are logged, not fatal — the participation row
/// already records the invitation.
async fn publish_invites(
    ctx: &PostContext,
    status: &posts::StatusRow,
    organizer_account_id: i64,
    invitee_ids: &[i64],
) -> Result<()> {
    if invitee_ids.is_empty() {
        return Ok(());
    }
    let Some(ap) = &ctx.ap else {
        return Ok(());
    };
    let event_uri = status
        .uri
        .clone()
        .ok_or_else(|| Error::Internal("local event without uri".to_string()))?;
    let (organizer_actor, username) = actor_of(&ctx.db, organizer_account_id).await?;
    let actors: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, actor_id FROM accounts WHERE id = ANY($1)")
            .bind(invitee_ids)
            .fetch_all(&ctx.db)
            .await?;
    for (invitee, actor) in actors {
        let doc = OutboundActivity::Invite {
            object: event_uri.clone(),
            to: vec![actor],
            cc: vec![],
        }
        .to_json(&organizer_actor);
        let invite_uri = doc.get("id").and_then(|v| v.as_str()).map(str::to_string);
        match crate::activitypub::delivery::publish(ap, &username, &doc).await {
            Ok(_) => {
                if let Some(uri) = invite_uri {
                    if let Err(e) = sqlx::query(
                        "UPDATE event_participations SET invite_activity_uri = $3, \
                         updated_at = now() \
                         WHERE status_id = $1 AND account_id = $2",
                    )
                    .bind(status.id)
                    .bind(invitee)
                    .bind(&uri)
                    .execute(&ctx.db)
                    .await
                    {
                        tracing::warn!(status_id = status.id, invitee, error = %e,
                            "invite activity uri stamp failed");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(status_id = status.id, invitee, error = %e,
                    "event invite publish failed")
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `account_ids` accepts the string wire form entity ids use —
    /// snowflakes exceed JS's safe-integer range.
    #[test]
    fn invitees_body_accepts_string_ids() {
        let body: InviteesBody =
            serde_json::from_value(json!({"account_ids": ["117518582417719632", 7]})).unwrap();
        let ids: Vec<i64> = body
            .account_ids
            .iter()
            .map(EntityId::to_i64)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, vec![117518582417719632, 7]);
    }
}
