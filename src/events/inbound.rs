//! Inbound RSVP activities for events (plan.md §7, contract Section
//! 10): `Join`, `Leave`, `Undo{Join}`, `TentativeAccept`, `Ignore`,
//! and the `Accept`/`Reject` branches that answer our `Invite`s or
//! our outbound `Join`s.
//!
//! Every arm resolves the event through the object URI
//! (`Join.object` **or** `Join.target` — servers disagree), loads the
//! `events` row + the actor's participation, runs the pure
//! [`rsvp::transition`] and applies the outcome in one transaction
//! ([`rsvp::apply`]). Outbound activities publish through a fresh
//! per-username resolver — inbound callbacks only hold `&ApCore`,
//! not the `ApContext` [`delivery::publish`] wants.
//!
//! [`select_remote_protocol`] runs post-ingest (Create **and**
//! Update) to pick `events.remote_join_protocol` per §7.3.

use std::sync::Arc;

use pubby_sdk::Activity;
use serde_json::Value;
use sqlx::PgPool;

use crate::{
    activitypub::{actors, nodeinfo, ApCore},
    events::{entity, model::*, rsvp},
    interactions, posts, Result,
};

/// Entry point for `Join`/`Leave`/`TentativeAccept`/`Ignore` arms in
/// `on_activity_fn`. Returns `true` when the activity concerned an
/// event (consumed); `false` lets pubby's dispatch see it.
pub async fn handle_activity(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<bool> {
    match activity.activity_type.as_str() {
        "Join" => handle_join(core, activity, raw).await,
        "Leave" => handle_leave(core, activity, raw).await,
        "TentativeAccept" => {
            handle_response(
                core,
                activity,
                raw,
                rsvp::RemoteResponseKind::TentativeAccept,
            )
            .await
        }
        "Ignore" => handle_response(core, activity, raw, rsvp::RemoteResponseKind::Ignore).await,
        _ => Ok(false),
    }
}

/// `Undo{Join}` arm — routed from `undo_inbound`'s inner-type match.
/// `activity` is the signed outer `Undo`: its actor is the verified
/// joiner leaving. The embedded Join's `actor` is self-asserted and
/// must not be trusted (a signer could otherwise remove someone
/// else's participation by naming them inside the Undo).
pub async fn handle_undo_join(core: &ApCore, activity: &Activity, inner: &Value) -> Result<bool> {
    let Some(uri) = rsvp_target_uri(inner) else {
        return Ok(false);
    };
    let Some((status, _event)) = local_event_by_uri(core, &uri).await? else {
        return Ok(false);
    };
    leave_local(core, &status, &activity.actor, &uri).await?;
    Ok(true)
}

/// `Accept`/`Reject`/`TentativeAccept`/`Ignore` arms — event RSVPs
/// only; `Follow` responses, `QuoteAuthorization`s and everything
/// else return `false` so the interactions handler keeps its
/// dispatch. `kind` is the caller's activity type mapped onto
/// [`rsvp::RemoteResponseKind`].
pub async fn handle_response(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
    kind: rsvp::RemoteResponseKind,
) -> Result<bool> {
    if local_event_response(core, activity, raw, kind).await? {
        return Ok(true);
    }
    remote_event_response(core, activity, raw, kind).await
}

/// The event URI an RSVP activity points at — `object` first, then
/// `target` (FEP servers disagree about which carries the event).
fn rsvp_target_uri(raw: &Value) -> Option<String> {
    interactions::object_uri(raw.get("object"))
        .or_else(|| interactions::object_uri(raw.get("target")))
}

/// Load `(status, event row)` for a URI — only *local* events take
/// inbound joins; remote events return `None` here (their responses
/// are handled by [`remote_event_response`]).
async fn local_event_by_uri(
    core: &ApCore,
    uri: &str,
) -> Result<Option<(posts::StatusRow, EventRow)>> {
    let Some(status) = posts::load_by_uri(&core.db, uri).await? else {
        return Ok(None);
    };
    if !status.local || status.tombstone || !posts::is_event(&status) {
        return Ok(None);
    }
    let bundles = entity::load_many(&core.db, &[status.id], None).await?;
    Ok(bundles
        .get(&status.id)
        .map(|b| (status.clone(), b.row.clone())))
}

/// The actor's `accounts.id` — remote actors upsert through the same
/// path status ingest uses.
async fn ensure_actor(core: &ApCore, actor_uri: &str) -> Result<Option<i64>> {
    match posts::remote::ensure_account(core, actor_uri).await {
        Ok(id) => Ok(Some(id)),
        Err(e) => {
            tracing::warn!(error = %e, actor = %actor_uri, "rsvp actor upsert failed");
            Ok(None)
        }
    }
}

/// Whether `actor_id` is blocked (either direction), domain-blocked
/// by the organizer, or suspended — the §7.1 silent-drop set.
async fn actor_blocked(core: &ApCore, organizer_id: i64, actor_id: i64) -> Result<bool> {
    if interactions::is_blocked(&core.db, organizer_id, actor_id).await? {
        return Ok(true);
    }
    let suspended: bool = sqlx::query_scalar(
        "SELECT suspended_at IS NOT NULL OR deleted_at IS NOT NULL FROM accounts WHERE id = $1",
    )
    .bind(actor_id)
    .fetch_one(&core.db)
    .await?;
    Ok(suspended)
}

/// `(organizer actor uri, organizer username)` for the event author.
async fn organizer_actor(core: &ApCore, account_id: i64) -> Result<(String, String)> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT actor_id, username FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&core.db)
            .await?;
    Ok(row.unwrap_or_default())
}

/// The actor's participation row on `status_id`, if any. Generic over
/// the executor so it can run on the pool or inside a transaction.
async fn participation<'e, E>(
    ex: E,
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
    .fetch_optional(ex)
    .await
    .map_err(Into::into)
}

/// Decide and apply an RSVP outcome atomically: the `events` row is
/// locked `FOR UPDATE` *first*, then the event row and the actor's
/// participation are re-read inside the same transaction before
/// [`rsvp::transition`] runs — so capacity checks and counter deltas
/// can't race a concurrent RSVP (review C-4). `party.invited` is
/// derived from the locked participation row, not a stale read.
///
/// Returns the outcome for post-commit publishing, or `None` when the
/// transition dropped the activity (nothing was written; the tx rolls
/// back on drop).
async fn decide_and_apply(
    core: &ApCore,
    status_id: i64,
    account_id: i64,
    input: rsvp::RsvpInput,
    mut party: rsvp::RsvpParty,
    env: &rsvp::RsvpEnv,
) -> Result<Option<rsvp::RsvpOutcome>> {
    let mut tx = core.db.begin().await?;
    let event =
        sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE status_id = $1 FOR UPDATE")
            .bind(status_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| crate::Error::NotFound("event".into()))?;
    let existing = participation(&mut *tx, status_id, account_id).await?;
    party.invited = existing
        .as_ref()
        .is_some_and(|e| e.state == ParticipationState::Invited);
    let Some(outcome) = rsvp::transition(&event, existing.as_ref(), input, &party, env)? else {
        return Ok(None);
    };
    rsvp::apply(&mut tx, status_id, account_id, &outcome).await?;
    tx.commit().await?;
    Ok(Some(outcome))
}

/// Publish an outbound RSVP activity through the sender's handler —
/// a fresh resolver since inbound callbacks hold `&ApCore` only.
async fn publish_outbound(core: &Arc<ApCore>, username: &str, activity: &Value) -> Result<()> {
    let resolver = actors::make_resolver(core.clone(), actors::HandlerCache::default());
    let username = username.to_string();
    let activity = activity.clone();
    actors::sync_call(move || {
        let handler = resolver(&username)
            .map_err(crate::Error::Federation)?
            .ok_or_else(|| crate::Error::NotFound(format!("actor {username}")))?;
        handler
            .publish_activity(&activity)
            .map(|_| ())
            .map_err(crate::Error::Federation)
    })
    .await
}

/// `Join{object|target: local event}` — run the §7.1 transition.
async fn handle_join(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<bool> {
    let Some(uri) = rsvp_target_uri(raw) else {
        return Ok(false);
    };
    let Some((status, _event)) = local_event_by_uri(core, &uri).await? else {
        return Ok(false);
    };
    let Some(actor_id) = ensure_actor(core, &activity.actor).await? else {
        return Ok(true);
    };
    if actor_blocked(core, status.account_id, actor_id).await? {
        return Ok(true);
    }
    let (organizer_actor, organizer_username) = organizer_actor(core, status.account_id).await?;
    let party = rsvp::RsvpParty {
        account_id: actor_id,
        actor_uri: activity.actor.clone(),
        username: None,
        blocked: false, // already checked above
        invited: false, // set from the locked row in decide_and_apply
        authorized_responder: false,
    };
    let env = rsvp::RsvpEnv {
        event_local: true,
        organizer_actor: organizer_actor.clone(),
        attributed_to_actor: None,
        required_join_visibility: vec![],
        organizer_actors: vec![],
        our_actor: organizer_actor.clone(),
        event_uri: status.uri.clone().unwrap_or_else(|| uri.clone()),
        now: chrono::Utc::now(),
    };
    // `participationMessage` rides on the Join's embedded object.
    let message = raw
        .get("object")
        .and_then(|o| o.get("participationMessage"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let input = rsvp::RsvpInput::Join {
        join_uri: (!activity.id.is_empty()).then(|| activity.id.clone()),
        participation_message: message,
    };
    let Some(outcome) = decide_and_apply(core, status.id, actor_id, input, party, &env).await?
    else {
        return Ok(true);
    };
    if let Some(outbound) = &outcome.outbound {
        let doc = outbound.to_json(&organizer_actor);
        if let Err(e) = publish_outbound(core, &organizer_username, &doc).await {
            tracing::warn!(error = %e, "rsvp outbound publish failed");
        }
    }
    Ok(true)
}

/// `Leave{object: local event}` — remove the participation; no
/// outbound, no notification (§7.1).
async fn handle_leave(core: &Arc<ApCore>, activity: &Activity, raw: &Value) -> Result<bool> {
    let Some(uri) = rsvp_target_uri(raw) else {
        return Ok(false);
    };
    let Some((status, _event)) = local_event_by_uri(core, &uri).await? else {
        return Ok(false);
    };
    leave_local(core, &status, &activity.actor, &uri).await?;
    Ok(true)
}

/// Shared leave path for `Leave` and `Undo{Join}`.
async fn leave_local(
    core: &ApCore,
    status: &posts::StatusRow,
    actor_uri: &str,
    uri: &str,
) -> Result<()> {
    let Some(actor_id) = ensure_actor(core, actor_uri).await? else {
        return Ok(());
    };
    let (organizer_actor, _) = organizer_actor(core, status.account_id).await?;
    let party = rsvp::RsvpParty {
        account_id: actor_id,
        actor_uri: actor_uri.to_string(),
        ..Default::default()
    };
    let env = rsvp::RsvpEnv {
        event_local: true,
        organizer_actor,
        attributed_to_actor: None,
        required_join_visibility: vec![],
        organizer_actors: vec![],
        our_actor: String::new(),
        event_uri: status.uri.clone().unwrap_or_else(|| uri.to_string()),
        now: chrono::Utc::now(),
    };
    decide_and_apply(
        core,
        status.id,
        actor_id,
        rsvp::RsvpInput::Leave,
        party,
        &env,
    )
    .await?;
    Ok(())
}

/// Friendica-style `Accept`/`TentativeAccept`/`Reject` on a *local*
/// event or our `Invite` — resolves an `invited`/`pending`
/// participation via `invite_response`. Returns `false` when the
/// object isn't a local event (the remote branch tries next).
async fn local_event_response(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
    kind: rsvp::RemoteResponseKind,
) -> Result<bool> {
    let Some(object) = raw.get("object") else {
        return Ok(false);
    };
    let candidates: Vec<String> = match object {
        Value::String(s) => vec![s.clone()],
        Value::Object(_) => interactions::object_uri(Some(object)).into_iter().collect(),
        _ => vec![],
    };
    for uri in candidates {
        let Some((status, _event)) = local_event_by_uri(core, &uri).await? else {
            continue;
        };
        let Some(actor_id) = ensure_actor(core, &activity.actor).await? else {
            return Ok(true);
        };
        if kind == rsvp::RemoteResponseKind::Ignore {
            // `Ignore` on a *local* event is meaningless — we never
            // send a `Join` for our own events, so there is no
            // request to settle. Consume it without touching
            // participation state (a raw `pending → ignored` write
            // would drift `pending_count`; review W-1).
            return Ok(true);
        }
        let activity_uri = (!activity.id.is_empty()).then(|| activity.id.clone());
        let input = match kind {
            rsvp::RemoteResponseKind::Accept => rsvp::RsvpInput::AcceptInvite { activity_uri },
            rsvp::RemoteResponseKind::TentativeAccept => {
                rsvp::RsvpInput::TentativeAcceptInvite { activity_uri }
            }
            rsvp::RemoteResponseKind::Reject => rsvp::RsvpInput::RejectInvite { activity_uri },
            rsvp::RemoteResponseKind::Ignore => unreachable!(),
        };
        let (organizer_actor, _) = organizer_actor(core, status.account_id).await?;
        let party = rsvp::RsvpParty {
            account_id: actor_id,
            actor_uri: activity.actor.clone(),
            ..Default::default()
        };
        let env = rsvp::RsvpEnv {
            event_local: true,
            organizer_actor,
            attributed_to_actor: None,
            required_join_visibility: vec![],
            organizer_actors: vec![],
            our_actor: String::new(),
            event_uri: status.uri.clone().unwrap_or_else(|| uri.clone()),
            now: chrono::Utc::now(),
        };
        decide_and_apply(core, status.id, actor_id, input, party, &env).await?;
        return Ok(true);
    }
    Ok(false)
}

/// `Accept`/`Reject`/`Ignore`/`TentativeAccept` answering *our*
/// outbound `Join` on a remote event — §7.2 matching rules. Returns
/// `false` when nothing matches (the activity belongs elsewhere).
async fn remote_event_response(
    core: &Arc<ApCore>,
    activity: &Activity,
    raw: &Value,
    kind: rsvp::RemoteResponseKind,
) -> Result<bool> {
    let Some(object) = raw.get("object") else {
        return Ok(false);
    };
    // Rule 1: the object (string or embedded `id`) equals a stored
    // `request_activity_uri` on a remote event's participation row.
    let object_uri_str = interactions::object_uri(Some(object));
    let mut candidates: Vec<(i64, i64)> = match &object_uri_str {
        Some(id) => {
            sqlx::query_as(
                "SELECT p.status_id, p.account_id FROM event_participations p \
             JOIN statuses s ON s.id = p.status_id \
             WHERE p.request_activity_uri = $1 AND NOT s.local AND NOT s.tombstone",
            )
            .bind(id)
            .fetch_all(&core.db)
            .await?
        }
        None => vec![],
    };
    // Rule 2 fallback: embedded `Join{actor: our actor, object:
    // event}` — resolve the event and our account, then match the
    // participation row; the responder must be authorized (§7.2).
    if candidates.is_empty() && object.get("type").and_then(Value::as_str) == Some("Join") {
        if let (Some(join_actor), Some(event_uri)) = (
            object.get("actor").and_then(Value::as_str),
            interactions::object_uri(object.get("object")),
        ) {
            if let Some(account_id) = local_account_by_actor(&core.db, join_actor).await? {
                if let Some(status) = posts::load_by_uri(&core.db, &event_uri).await? {
                    if !status.local && !status.tombstone && posts::is_event(&status) {
                        candidates.push((status.id, account_id));
                    }
                }
            }
        }
    }
    for (status_id, account_id) in candidates {
        let Some(status) = posts::load(&core.db, status_id).await? else {
            continue;
        };
        if status.local || status.tombstone || !posts::is_event(&status) {
            continue;
        }
        let Some(bundle) = entity::load_many(&core.db, &[status_id], None)
            .await?
            .remove(&status_id)
        else {
            continue;
        };
        let Some(existing) = participation(&core.db, status_id, account_id).await? else {
            continue;
        };
        // §7.2 authorization for the fallback path (rule 1 already
        // matched the stored uri): organizer / attributedTo /
        // organizer collection / same host as the event id.
        let (organizer_actor, _) = organizer_actor(core, status.account_id).await?;
        let event_uri = status.uri.clone().unwrap_or_default();
        let responder_host = crate::activitypub::moderation::actor_domain(&activity.actor);
        let event_host = crate::activitypub::moderation::actor_domain(&event_uri);
        let organizer_actors: Vec<String> = bundle
            .organizer_accounts
            .values()
            .map(|a| a.actor_id.clone())
            .collect();
        let direct = object_uri_str
            .as_deref()
            .is_some_and(|id| existing.request_activity_uri.as_deref() == Some(id));
        let authorized = organizer_actor == activity.actor
            || organizer_actors.contains(&activity.actor)
            || (responder_host.is_some() && responder_host == event_host);
        let fallback = !direct
            && authorized
            && object.get("type").and_then(Value::as_str) == Some("Join")
            && existing_actor_uri(&core.db, account_id)
                .await?
                .is_some_and(|our| {
                    object.get("actor").and_then(Value::as_str) == Some(our.as_str())
                })
            && interactions::object_uri(object.get("object")).is_some_and(|o| o == event_uri);
        if !direct && !fallback {
            continue;
        }
        let party = rsvp::RsvpParty {
            account_id,
            actor_uri: activity.actor.clone(),
            authorized_responder: authorized,
            ..Default::default()
        };
        let input = rsvp::RsvpInput::RemoteResponse {
            kind,
            activity_uri: (!activity.id.is_empty()).then(|| activity.id.clone()),
        };
        let env = rsvp::RsvpEnv {
            event_local: false,
            organizer_actor,
            attributed_to_actor: None,
            required_join_visibility: vec![],
            organizer_actors,
            our_actor: String::new(),
            event_uri,
            now: chrono::Utc::now(),
        };
        // The transition re-reads the event row + participation
        // under the `events` lock inside `decide_and_apply` — the
        // `existing`/`bundle.row` reads above are for matching only.
        decide_and_apply(core, status_id, account_id, input, party, &env).await?;
        return Ok(true);
    }
    Ok(false)
}

/// The account's actor URI.
async fn existing_actor_uri(db: &PgPool, account_id: i64) -> Result<Option<String>> {
    sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(db)
        .await
        .map_err(Into::into)
}

/// Local (`domain IS NULL`) account id by actor URI.
async fn local_account_by_actor(db: &PgPool, actor_uri: &str) -> Result<Option<i64>> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1 AND domain IS NULL")
        .bind(actor_uri)
        .fetch_optional(db)
        .await
        .map_err(Into::into)
}

/// §7.3 — pick `events.remote_join_protocol` for a freshly
/// materialized/updated remote event. Best-effort: failures log and
/// leave the stored value.
///
/// Rules: `external`/`none` joinMode short-circuit; an existing
/// `invite_response` (the event arrived via `Invite`, proven by
/// invited participation rows) is kept; nodeinfo `friendica`/
/// `hubzilla` → `invite_response`, `gancio` → `none`; everything else
/// → `join`.
pub async fn select_remote_protocol(core: &ApCore, status_id: i64) -> Result<()> {
    let row = sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE status_id = $1")
        .bind(status_id)
        .fetch_optional(&core.db)
        .await?;
    let Some(row) = row else {
        return Ok(());
    };
    // Local events never take remote RSVPs.
    let is_local: bool = sqlx::query_scalar("SELECT local FROM statuses WHERE id = $1")
        .bind(status_id)
        .fetch_one(&core.db)
        .await?;
    if is_local {
        return Ok(());
    }
    let protocol = match row.join_mode {
        JoinMode::External => RemoteJoinProtocol::External,
        JoinMode::None => RemoteJoinProtocol::None,
        _ => {
            let invited: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM event_participations \
                 WHERE status_id = $1 AND protocol = 'invite_response')",
            )
            .bind(status_id)
            .fetch_one(&core.db)
            .await?;
            if invited || row.remote_join_protocol == Some(RemoteJoinProtocol::InviteResponse) {
                RemoteJoinProtocol::InviteResponse
            } else {
                let status = posts::load(&core.db, status_id).await?;
                let host = status
                    .as_ref()
                    .and_then(|s| s.uri.clone())
                    .and_then(|u| crate::activitypub::moderation::actor_domain(&u));
                match host {
                    Some(host) => {
                        let software = nodeinfo::software_of(core, &host).await;
                        match software.as_deref() {
                            Some("friendica") | Some("hubzilla") => {
                                RemoteJoinProtocol::InviteResponse
                            }
                            Some("gancio") => {
                                // Gancio has no Join support — keep
                                // the event URL as the external
                                // participation link (never
                                // overwriting an organizer-set one).
                                let link = if row.external_participation_url.is_none() {
                                    status
                                        .as_ref()
                                        .and_then(|s| s.url.clone().or_else(|| s.uri.clone()))
                                } else {
                                    None
                                };
                                if let Some(url) = link {
                                    sqlx::query(
                                        "UPDATE events SET external_participation_url = $2 \
                                         WHERE status_id = $1 \
                                         AND external_participation_url IS NULL",
                                    )
                                    .bind(status_id)
                                    .bind(&url)
                                    .execute(&core.db)
                                    .await?;
                                }
                                RemoteJoinProtocol::None
                            }
                            _ => RemoteJoinProtocol::Join,
                        }
                    }
                    None => RemoteJoinProtocol::Join,
                }
            }
        }
    };
    if row.remote_join_protocol != Some(protocol) {
        sqlx::query("UPDATE events SET remote_join_protocol = $2 WHERE status_id = $1")
            .bind(status_id)
            .bind(protocol.to_string())
            .execute(&core.db)
            .await?;
    }
    Ok(())
}
