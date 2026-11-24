//! RSVP state machine for events (plan.md §7.1–7.3).
//!
//! [`transition`] is a pure function: it maps
//! `(event, existing participation, input, party, env)` to an
//! [`RsvpOutcome`] — the participation write, counter deltas, an
//! optional outbound AP activity and notification hints. All DB and
//! network facts the decision needs (blocked? invited? authorized
//! responder? event locality? clock) are resolved by the caller into
//! [`RsvpParty`]/[`RsvpEnv`], which keeps the matrix exhaustively
//! unit-testable.
//!
//! [`apply`] performs the outcome inside one transaction with
//! `SELECT … FOR UPDATE` on the `events` row — the only place
//! `participants_count`/`pending_count` may change. Organizer rows
//! (`role='organizer'`) never count toward capacity.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::{
    events::model::{
        EventParticipationRow, EventRow, JoinMode, ParticipationProtocol, ParticipationState,
    },
    Result,
};

/// Facts about the counterparty the pure transition cannot look up.
#[derive(Debug, Clone, Default)]
pub struct RsvpParty {
    /// The counterparty's `accounts.id` (joiner, our user, or the
    /// response sender).
    pub account_id: i64,
    /// The counterparty's actor URI.
    pub actor_uri: String,
    /// Local username — set for local accounts so outbound activities
    /// can be published through their handler.
    pub username: Option<String>,
    /// Blocked / domain-blocked / suspended relative to the organizer.
    pub blocked: bool,
    /// Holds an invitation: an `invited` participation row, a drained
    /// pending invite, or (for local invitees) an invite record.
    pub invited: bool,
    /// For responses to *our* outbound Join: the responder is an
    /// event organizer, the `attributedTo`, or on the same host as the
    /// event id (plan.md §7.2 matching rule 2).
    pub authorized_responder: bool,
}

/// Environment facts the transition needs.
#[derive(Debug, Clone)]
pub struct RsvpEnv {
    /// Event locality — `statuses.local` of the event's status.
    /// Local events take inbound `Join`s; remote events take our
    /// outbound `Join`/`Leave` and inbound responses.
    pub event_local: bool,
    /// The organizer actor URI (status author) — `cc`/`to` fallback.
    pub organizer_actor: String,
    /// `attributedTo` actor URI when it differs from the organizer.
    pub attributed_to_actor: Option<String>,
    /// `requiredJoinVisibility` actors on the remote event.
    pub required_join_visibility: Vec<String>,
    /// Organizer actor URIs disclosed on the event (remote
    /// `organizers` collection) — Join/Leave `to` recipients.
    pub organizer_actors: Vec<String>,
    /// Our actor URI (the local account acting) — response matching.
    pub our_actor: String,
    /// The event's AP URI.
    pub event_uri: String,
    /// Clock — "ended event" checks.
    pub now: DateTime<Utc>,
}

/// One RSVP input — local API actions and inbound AP activities
/// share the same enum (the caller normalizes).
#[derive(Debug, Clone)]
pub enum RsvpInput {
    /// `Join` — inbound on a local event, or our user joining a
    /// remote one. `join_uri` is the Join activity's own id (the
    /// object remote `Accept`/`Reject`/`Ignore` answer).
    /// `tentative` marks a tentative join (local API only — remote
    /// `Join`s have no tentative flag).
    Join {
        join_uri: Option<String>,
        participation_message: Option<String>,
        tentative: bool,
    },
    /// `Leave` / `Undo{Join}`.
    Leave,
    /// Local opt-out record — writes `declined` without federating
    /// (no pending join to answer).
    Decline,
    /// Friendica-style `Accept` on our `Invite` or the event itself.
    AcceptInvite { activity_uri: Option<String> },
    /// Friendica-style `TentativeAccept`.
    TentativeAcceptInvite { activity_uri: Option<String> },
    /// Friendica-style `Reject`.
    RejectInvite { activity_uri: Option<String> },
    /// Inbound `Accept`/`Reject`/`Ignore`/`TentativeAccept` answering
    /// our outbound `Join` on a remote event. `activity_uri` is the
    /// responder's activity id (stored as `response_activity_uri`).
    RemoteResponse {
        kind: RemoteResponseKind,
        activity_uri: Option<String>,
    },
}

/// Response kinds for [`RsvpInput::RemoteResponse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteResponseKind {
    Accept,
    Reject,
    Ignore,
    TentativeAccept,
}

/// The participation row to write — `None` deletes the row.
#[derive(Debug, Clone)]
pub struct ParticipationWrite {
    pub state: ParticipationState,
    pub protocol: ParticipationProtocol,
    pub request_activity_uri: Option<String>,
    pub response_activity_uri: Option<String>,
    pub invite_activity_uri: Option<String>,
    pub participation_message: Option<String>,
}

impl ParticipationWrite {
    fn keep(existing: &EventParticipationRow) -> Self {
        Self {
            state: existing.state,
            protocol: existing.protocol,
            request_activity_uri: existing.request_activity_uri.clone(),
            response_activity_uri: existing.response_activity_uri.clone(),
            invite_activity_uri: existing.invite_activity_uri.clone(),
            participation_message: existing.participation_message.clone(),
        }
    }
}

/// Counter deltas applied under the `events` row lock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterDelta {
    pub accepted: i32,
    pub pending: i32,
}

/// An outbound ActivityPub activity to publish. `to_json` stamps the
/// FEP-8a8e `@context` and the §5.3 addressing.
#[derive(Debug, Clone)]
pub enum OutboundActivity {
    /// `Join{object: event_uri, participationMessage?}` — our user
    /// joining a remote event.
    Join {
        event_uri: String,
        participation_message: Option<String>,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `Leave{object: event_uri}`.
    Leave {
        event_uri: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `Accept{object: join_id}` — answer to a remote `Join`.
    Accept {
        object: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `Reject{object: join_id}`.
    Reject {
        object: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `Ignore{object: join_id}` (FEP MUST for `none`/`external`).
    Ignore {
        object: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `TentativeAccept{object: invite_id|event_uri}` — Friendica-style
    /// response to an invite/event.
    TentativeAccept {
        object: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
    /// `Invite{object: event_uri, to: [invitee]}` — the organizer
    /// inviting one account to an invitees-only event (§16).
    Invite {
        object: String,
        to: Vec<String>,
        cc: Vec<String>,
    },
}

impl OutboundActivity {
    /// Render the activity JSON. `actor` is our actor URI; the
    /// activity id is `{actor}#{kind}/{uuid}` (dereferenceable through
    /// the actor's outbox/ap paths like other hand-built activities).
    pub fn to_json(&self, actor: &str) -> Value {
        let (kind, object, extra) = match self {
            Self::Join {
                event_uri,
                participation_message,
                ..
            } => (
                "join",
                json!(event_uri),
                participation_message
                    .as_ref()
                    .map(|m| json!({"participationMessage": m}))
                    .unwrap_or(Value::Null),
            ),
            Self::Leave { event_uri, .. } => ("leave", json!(event_uri), Value::Null),
            Self::Accept { object, .. } => ("accept", json!(object), Value::Null),
            Self::Reject { object, .. } => ("reject", json!(object), Value::Null),
            Self::Ignore { object, .. } => ("ignore", json!(object), Value::Null),
            Self::TentativeAccept { object, .. } => ("tentativeaccept", json!(object), Value::Null),
            Self::Invite { object, .. } => ("invite", json!(object), Value::Null),
        };
        let mut doc = json!({
            "@context": crate::activitypub::context::event_context(),
            "id": format!("{actor}#{kind}/{}", uuid::Uuid::new_v4()),
            "type": match self {
                Self::Join { .. } => "Join",
                Self::Leave { .. } => "Leave",
                Self::Accept { .. } => "Accept",
                Self::Reject { .. } => "Reject",
                Self::Ignore { .. } => "Ignore",
                Self::TentativeAccept { .. } => "TentativeAccept",
                Self::Invite { .. } => "Invite",
            },
            "actor": actor,
            "object": object,
        });
        if let (Some(to), Some(cc)) = (self.to(), self.cc()) {
            doc["to"] = json!(to);
            doc["cc"] = json!(cc);
        }
        if extra.is_object() {
            doc.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
        }
        doc
    }

    fn to(&self) -> Option<&Vec<String>> {
        match self {
            Self::Join { to, .. }
            | Self::Leave { to, .. }
            | Self::Accept { to, .. }
            | Self::Reject { to, .. }
            | Self::Ignore { to, .. }
            | Self::TentativeAccept { to, .. }
            | Self::Invite { to, .. } => Some(to),
        }
    }

    fn cc(&self) -> Option<&Vec<String>> {
        match self {
            Self::Join { cc, .. }
            | Self::Leave { cc, .. }
            | Self::Accept { cc, .. }
            | Self::Reject { cc, .. }
            | Self::Ignore { cc, .. }
            | Self::TentativeAccept { cc, .. }
            | Self::Invite { cc, .. } => Some(cc),
        }
    }
}

/// What [`transition`] decided.
#[derive(Debug)]
pub struct RsvpOutcome {
    /// Participation write — `None` deletes the row.
    pub participation: Option<ParticipationWrite>,
    /// Counter deltas (applied under the events row lock).
    pub counters: CounterDelta,
    /// Outbound activity to publish, if any.
    pub outbound: Option<OutboundActivity>,
    /// Notify the organizer(s) — pending join on a restricted event.
    pub notify_organizer: bool,
    /// Notify the participant — invite responses, organizer actions.
    pub notify_participant: bool,
    /// `Ignore` on a `join`-protocol remote event downgrades
    /// `remote_join_protocol` to `none`/`external`.
    pub downgrade_protocol: bool,
}

impl RsvpOutcome {
    fn write(
        participation: Option<ParticipationWrite>,
        counters: CounterDelta,
        outbound: Option<OutboundActivity>,
    ) -> Self {
        Self {
            participation,
            counters,
            outbound,
            notify_organizer: false,
            notify_participant: false,
            downgrade_protocol: false,
        }
    }
}

/// Whether the event has ended — `end_time`, else `start_time`,
/// compared to `env.now`. Postponed events (no start) never end.
fn event_ended(event: &EventRow, env: &RsvpEnv) -> bool {
    event
        .end_time
        .or(event.start_time)
        .is_some_and(|t| t < env.now)
}

/// Capacity check for `free` joins — organizer rows never count, and
/// `remaining_capacity` already excludes them by construction.
fn capacity_full(event: &EventRow) -> bool {
    event.is_capacity_limited() && event.remaining_capacity() == Some(0)
}

/// Join/Leave addressing for our outbound activities on a remote
/// event: `to` = organizer actors ∪ `requiredJoinVisibility`;
/// `cc` = `attributedTo` when it differs from every `to` member.
fn remote_audience(env: &RsvpEnv) -> (Vec<String>, Vec<String>) {
    let mut to: Vec<String> = env.organizer_actors.clone();
    for v in &env.required_join_visibility {
        if !to.contains(v) {
            to.push(v.clone());
        }
    }
    if to.is_empty() {
        to.push(env.organizer_actor.clone());
    }
    let mut cc = Vec::new();
    if let Some(attr) = &env.attributed_to_actor {
        if !to.contains(attr) && !cc.contains(attr) {
            cc.push(attr.clone());
        }
    }
    (to, cc)
}

/// Response addressing: `to` = the joiner; `cc` = `attributedTo`
/// when it differs from the organizer answering.
fn response_audience(env: &RsvpEnv, joiner: &str) -> (Vec<String>, Vec<String>) {
    let mut cc = Vec::new();
    if let Some(attr) = &env.attributed_to_actor {
        if attr != &env.organizer_actor && attr != joiner {
            cc.push(attr.clone());
        }
    }
    (vec![joiner.to_string()], cc)
}

/// The pure RSVP transition — see module docs for the contract.
/// `Ok(None)` means drop silently (blocked actor, organizer Leave,
/// unknown response).
pub fn transition(
    event: &EventRow,
    existing: Option<&EventParticipationRow>,
    incoming: RsvpInput,
    party: &RsvpParty,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    // Organizer rows are structural — Leave/Undo never deletes them.
    if let Some(e) = existing {
        if e.is_organizer() {
            return Ok(None);
        }
    }
    match incoming {
        RsvpInput::Join {
            join_uri,
            participation_message,
            tentative,
        } => join_transition(
            event,
            existing,
            join_uri,
            participation_message,
            tentative,
            party,
            env,
        ),
        RsvpInput::Leave => leave_transition(existing, env),
        RsvpInput::Decline => decline_transition(existing),
        RsvpInput::AcceptInvite { activity_uri } => {
            invite_response(existing, ParticipationState::Accepted, activity_uri, env)
        }
        RsvpInput::TentativeAcceptInvite { activity_uri } => {
            invite_response(existing, ParticipationState::Tentative, activity_uri, env)
        }
        RsvpInput::RejectInvite { activity_uri } => {
            invite_response(existing, ParticipationState::Declined, activity_uri, env)
        }
        RsvpInput::RemoteResponse { kind, activity_uri } => {
            remote_response(existing, kind, activity_uri, party)
        }
    }
}

fn join_transition(
    event: &EventRow,
    existing: Option<&EventParticipationRow>,
    join_uri: Option<String>,
    participation_message: Option<String>,
    tentative: bool,
    party: &RsvpParty,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    if env.event_local {
        local_join(
            event,
            existing,
            join_uri,
            participation_message,
            tentative,
            party,
            env,
        )
    } else {
        remote_join(event, existing, participation_message, env)
    }
}

/// §7.1 — inbound `Join` on a local event (or a local API join).
fn local_join(
    event: &EventRow,
    existing: Option<&EventParticipationRow>,
    join_uri: Option<String>,
    participation_message: Option<String>,
    tentative: bool,
    party: &RsvpParty,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    if party.blocked {
        return Ok(None);
    }
    // Idempotent re-Join by an accepted participant — re-send Accept.
    if let Some(e) = existing {
        if e.state == ParticipationState::Accepted {
            let (to, cc) = response_audience(env, &party.actor_uri);
            let object = e
                .request_activity_uri
                .clone()
                .or(join_uri.clone())
                .unwrap_or_else(|| env.event_uri.clone());
            return Ok(Some(RsvpOutcome::write(
                Some(ParticipationWrite::keep(e)),
                CounterDelta::default(),
                Some(OutboundActivity::Accept { object, to, cc }),
            )));
        }
    }
    if event_ended(event, env) || event.status == crate::events::model::EventStatus::Cancelled {
        let (to, cc) = response_audience(env, &party.actor_uri);
        let object = join_uri.clone().unwrap_or_else(|| env.event_uri.clone());
        return Ok(Some(RsvpOutcome::write(
            existing.map(ParticipationWrite::keep),
            CounterDelta::default(),
            Some(OutboundActivity::Reject { object, to, cc }),
        )));
    }
    let object = join_uri.clone().unwrap_or_else(|| env.event_uri.clone());
    let (to, cc) = response_audience(env, &party.actor_uri);
    match event.join_mode {
        JoinMode::None | JoinMode::External => Ok(Some(RsvpOutcome::write(
            existing.map(ParticipationWrite::keep),
            CounterDelta::default(),
            Some(OutboundActivity::Ignore { object, to, cc }),
        ))),
        JoinMode::Invite if !party.invited => Ok(Some(RsvpOutcome::write(
            existing.map(ParticipationWrite::keep),
            CounterDelta::default(),
            Some(OutboundActivity::Reject { object, to, cc }),
        ))),
        JoinMode::Free => {
            if capacity_full(event) {
                return Ok(Some(RsvpOutcome::write(
                    existing.map(ParticipationWrite::keep),
                    CounterDelta::default(),
                    Some(OutboundActivity::Reject { object, to, cc }),
                )));
            }
            let state = if tentative {
                ParticipationState::Tentative
            } else {
                ParticipationState::Accepted
            };
            let write = ParticipationWrite {
                state,
                // A local account joining through the API is `local`;
                // a remote joiner answering over AP is `join`.
                protocol: if party.username.is_some() {
                    ParticipationProtocol::Local
                } else {
                    ParticipationProtocol::Join
                },
                request_activity_uri: join_uri.clone(),
                response_activity_uri: None,
                invite_activity_uri: existing.and_then(|e| e.invite_activity_uri.clone()),
                participation_message: participation_message.clone(),
            };
            Ok(Some(RsvpOutcome::write(
                Some(write),
                counter_delta(existing, state),
                Some(if tentative {
                    OutboundActivity::TentativeAccept { object, to, cc }
                } else {
                    OutboundActivity::Accept { object, to, cc }
                }),
            )))
        }
        JoinMode::Restricted => {
            let write = ParticipationWrite {
                state: ParticipationState::Pending,
                protocol: ParticipationProtocol::Join,
                request_activity_uri: join_uri.clone(),
                response_activity_uri: None,
                invite_activity_uri: existing.and_then(|e| e.invite_activity_uri.clone()),
                participation_message: participation_message.clone(),
            };
            let mut out = RsvpOutcome::write(
                Some(write),
                counter_delta(existing, ParticipationState::Pending),
                None,
            );
            out.notify_organizer = true;
            Ok(Some(out))
        }
        JoinMode::Invite => {
            // `party.invited` — accept immediately.
            let state = if tentative {
                ParticipationState::Tentative
            } else {
                ParticipationState::Accepted
            };
            let write = ParticipationWrite {
                state,
                protocol: ParticipationProtocol::InviteResponse,
                request_activity_uri: join_uri.clone(),
                response_activity_uri: None,
                invite_activity_uri: existing.and_then(|e| e.invite_activity_uri.clone()),
                participation_message: participation_message.clone(),
            };
            Ok(Some(RsvpOutcome::write(
                Some(write),
                counter_delta(existing, state),
                Some(if tentative {
                    OutboundActivity::TentativeAccept { object, to, cc }
                } else {
                    OutboundActivity::Accept { object, to, cc }
                }),
            )))
        }
    }
}

/// Local `declined` write — records the opt-out without federating
/// (pending remote joins leave through [`RsvpInput::Leave`] instead).
fn decline_transition(existing: Option<&EventParticipationRow>) -> Result<Option<RsvpOutcome>> {
    let write = ParticipationWrite {
        state: ParticipationState::Declined,
        protocol: ParticipationProtocol::Local,
        request_activity_uri: None,
        response_activity_uri: None,
        invite_activity_uri: existing.and_then(|e| e.invite_activity_uri.clone()),
        participation_message: None,
    };
    Ok(Some(RsvpOutcome::write(
        Some(write),
        counter_delta(existing, ParticipationState::Declined),
        None,
    )))
}

/// Counter delta for moving `existing` → `new_state`: +1 for each
/// bucket gained, −1 for each lost. `participants_count` counts
/// `accepted` rows only; `pending_count` counts `pending`.
fn counter_delta(
    existing: Option<&EventParticipationRow>,
    new: ParticipationState,
) -> CounterDelta {
    let old = existing.map(|e| e.state);
    CounterDelta {
        accepted: i32::from(new == ParticipationState::Accepted)
            - i32::from(old == Some(ParticipationState::Accepted)),
        pending: i32::from(new == ParticipationState::Pending)
            - i32::from(old == Some(ParticipationState::Pending)),
    }
}

/// `Leave` / `Undo{Join}` — delete the participation, adjust
/// counters, no notification (plan §7.1).
fn leave_transition(
    existing: Option<&EventParticipationRow>,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    let Some(e) = existing else {
        return Ok(None);
    };
    // Our Leave on a remote event federates back.
    let outbound = if !env.event_local {
        let (to, cc) = remote_audience(env);
        Some(OutboundActivity::Leave {
            event_uri: env.event_uri.clone(),
            to,
            cc,
        })
    } else {
        None
    };
    Ok(Some(RsvpOutcome::write(
        None,
        counter_delta_for_removal(e),
        outbound,
    )))
}

/// Counter delta for deleting `e` — mirror of the write-side deltas.
fn counter_delta_for_removal(e: &EventParticipationRow) -> CounterDelta {
    CounterDelta {
        accepted: -i32::from(e.state == ParticipationState::Accepted),
        pending: -i32::from(e.state == ParticipationState::Pending),
    }
}

/// Friendica-style `Accept`/`TentativeAccept`/`Reject` on our Invite
/// or the event — resolves an `invited` row immediately
/// (`protocol='invite_response'`). On a *remote* event the answer
/// federates back to the organizer (§5.3: `to` = organizer actors);
/// the object is the stored `invite_activity_uri` when the invite
/// named one, else the event URI.
fn invite_response(
    existing: Option<&EventParticipationRow>,
    state: ParticipationState,
    activity_uri: Option<String>,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    let Some(e) = existing else {
        return Ok(None);
    };
    // Only `invited` rows resolve this way — a `pending` join on a
    // restricted event is the organizer's decision (approval queue,
    // Section 11), never the joiner's own `Accept`.
    if e.state != ParticipationState::Invited {
        return Ok(None);
    }
    let mut write = ParticipationWrite::keep(e);
    write.state = state;
    write.protocol = ParticipationProtocol::InviteResponse;
    write.response_activity_uri = activity_uri;
    let counters = counter_delta(existing, state);
    let outbound = if env.event_local {
        None
    } else {
        let object = e
            .invite_activity_uri
            .clone()
            .unwrap_or_else(|| env.event_uri.clone());
        let (to, cc) = invite_response_audience(env);
        Some(match state {
            ParticipationState::Accepted => OutboundActivity::Accept { object, to, cc },
            ParticipationState::Tentative => OutboundActivity::TentativeAccept { object, to, cc },
            ParticipationState::Declined => OutboundActivity::Reject { object, to, cc },
            _ => return Ok(None),
        })
    };
    let mut out = RsvpOutcome::write(Some(write), counters, outbound);
    out.notify_participant = true;
    Ok(Some(out))
}

/// Invite-response addressing for our outbound activities on a
/// remote event: `to` = the organizer actors (§5.3 — Friendica
/// requires `to`), `cc` = `attributedTo` when it differs.
fn invite_response_audience(env: &RsvpEnv) -> (Vec<String>, Vec<String>) {
    let mut to = env.organizer_actors.clone();
    if !to.contains(&env.organizer_actor) {
        to.push(env.organizer_actor.clone());
    }
    let mut cc = Vec::new();
    if let Some(attr) = &env.attributed_to_actor {
        if !to.contains(attr) {
            cc.push(attr.clone());
        }
    }
    (to, cc)
}

/// §7.2 — inbound `Accept`/`Reject`/`Ignore`/`TentativeAccept`
/// answering our outbound `Join` on a remote event. Matching itself
/// is [`response_matches`]; here we only apply the state change.
fn remote_response(
    existing: Option<&EventParticipationRow>,
    kind: RemoteResponseKind,
    activity_uri: Option<String>,
    party: &RsvpParty,
) -> Result<Option<RsvpOutcome>> {
    let Some(e) = existing else {
        return Ok(None);
    };
    // Only rows that represent *our* outbound request can be
    // answered — `request_activity_uri` set, protocol `join`, and the
    // responder must have passed [`response_matches`] upstream.
    if e.request_activity_uri.is_none() || e.account_id != party.account_id {
        return Ok(None);
    }
    let (state, downgrade) = match kind {
        RemoteResponseKind::Accept => (ParticipationState::Accepted, false),
        RemoteResponseKind::Reject => (ParticipationState::Rejected, false),
        RemoteResponseKind::TentativeAccept => (ParticipationState::Tentative, false),
        // `Ignore` settles the join as ignored and downgrades the
        // event's protocol to `none`/`external` (plan §7.3 rule 4).
        RemoteResponseKind::Ignore => (ParticipationState::Ignored, true),
    };
    let mut write = ParticipationWrite::keep(e);
    write.state = state;
    write.response_activity_uri = activity_uri;
    let counters = counter_delta(Some(e), state);
    let mut out = RsvpOutcome::write(Some(write), counters, None);
    out.downgrade_protocol = downgrade;
    Ok(Some(out))
}

/// Our user joining a remote event — `pending` under `join`, or
/// immediate resolution under `invite_response` (there is no invite
/// to answer, so we still emit `Join`; Friendica-style servers are
/// expected to answer with `Accept`/`Reject` either way).
fn remote_join(
    event: &EventRow,
    existing: Option<&EventParticipationRow>,
    participation_message: Option<String>,
    env: &RsvpEnv,
) -> Result<Option<RsvpOutcome>> {
    // Already participating — idempotent: nothing new to send.
    if let Some(e) = existing {
        if matches!(
            e.state,
            ParticipationState::Accepted
                | ParticipationState::Pending
                | ParticipationState::Tentative
        ) {
            return Ok(None);
        }
    }
    let protocol = match event.remote_join_protocol {
        Some(crate::events::model::RemoteJoinProtocol::InviteResponse) => {
            ParticipationProtocol::InviteResponse
        }
        _ => ParticipationProtocol::Join,
    };
    let write = ParticipationWrite {
        state: ParticipationState::Pending,
        protocol,
        request_activity_uri: None, // stamped post-publish — see stamp_request_uri
        response_activity_uri: None,
        invite_activity_uri: existing.and_then(|e| e.invite_activity_uri.clone()),
        participation_message: participation_message.clone(),
    };
    let (to, cc) = remote_audience(env);
    let outbound = OutboundActivity::Join {
        event_uri: env.event_uri.clone(),
        participation_message,
        to,
        cc,
    };
    Ok(Some(RsvpOutcome::write(
        Some(write),
        counter_delta(existing, ParticipationState::Pending),
        Some(outbound),
    )))
}

/// Apply an [`RsvpOutcome`] inside one transaction: upsert/delete the
/// participation row and adjust `events` counters under
/// `SELECT … FOR UPDATE`. Returns the affected participation id.
///
/// The caller publishes `outcome.outbound` *after* commit — the
/// activity id it stamps becomes `request_activity_uri`/`response`
/// bookkeeping for the next round.
pub async fn apply(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    status_id: i64,
    account_id: i64,
    outcome: &RsvpOutcome,
) -> Result<()> {
    // Lock the events row — counters serialize here.
    sqlx::query("SELECT status_id FROM events WHERE status_id = $1 FOR UPDATE")
        .bind(status_id)
        .execute(&mut **tx)
        .await?;
    let delta = outcome.counters;
    if delta.accepted != 0 || delta.pending != 0 {
        sqlx::query(
            "UPDATE events SET participants_count = GREATEST(participants_count + $2, 0), \
                 pending_count = GREATEST(pending_count + $3, 0), updated_at = now() \
             WHERE status_id = $1",
        )
        .bind(status_id)
        .bind(delta.accepted)
        .bind(delta.pending)
        .execute(&mut **tx)
        .await?;
    }
    match &outcome.participation {
        None => {
            sqlx::query(
                "DELETE FROM event_participations WHERE status_id = $1 AND account_id = $2",
            )
            .bind(status_id)
            .bind(account_id)
            .execute(&mut **tx)
            .await?;
        }
        Some(w) => {
            sqlx::query(
                "INSERT INTO event_participations \
                     (status_id, account_id, state, protocol, request_activity_uri, \
                      response_activity_uri, invite_activity_uri, participation_message) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
                 ON CONFLICT (status_id, account_id) DO UPDATE SET \
                     state = EXCLUDED.state, protocol = EXCLUDED.protocol, \
                     request_activity_uri = COALESCE(EXCLUDED.request_activity_uri, \
                         event_participations.request_activity_uri), \
                     response_activity_uri = EXCLUDED.response_activity_uri, \
                     invite_activity_uri = COALESCE(EXCLUDED.invite_activity_uri, \
                         event_participations.invite_activity_uri), \
                     participation_message = EXCLUDED.participation_message, \
                     updated_at = now()",
            )
            .bind(status_id)
            .bind(account_id)
            .bind(w.state.to_string())
            .bind(w.protocol.to_string())
            .bind(&w.request_activity_uri)
            .bind(&w.response_activity_uri)
            .bind(&w.invite_activity_uri)
            .bind(&w.participation_message)
            .execute(&mut **tx)
            .await?;
        }
    }
    if outcome.downgrade_protocol {
        // §7.3 rule 4 — an `Ignore` settles the event as
        // non-RSVP: `none`, or `external` when a participation URL
        // exists.
        sqlx::query(
            "UPDATE events SET remote_join_protocol = \
                 CASE WHEN external_participation_url IS NOT NULL THEN 'external' ELSE 'none' END \
             WHERE status_id = $1 AND remote_join_protocol = 'join'",
        )
        .bind(status_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Record the outbound `Join`'s activity id as
/// `request_activity_uri` — call after the Join is published so a
/// later `Accept`/`Reject`/`Ignore` can match it (§7.2 rule 1). The
/// local join API (Section 11) is the caller; `apply` can't stamp it
/// because the activity id only exists once `to_json` renders.
pub async fn stamp_request_uri(
    db: &sqlx::PgPool,
    status_id: i64,
    account_id: i64,
    activity_uri: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE event_participations SET request_activity_uri = $3, updated_at = now() \
         WHERE status_id = $1 AND account_id = $2",
    )
    .bind(status_id)
    .bind(account_id)
    .bind(activity_uri)
    .execute(db)
    .await?;
    Ok(())
}

/// §7.2 response matching — does `object` (string or embedded doc)
/// answer the Join stored as `request_activity_uri`?
///
/// 1. `object` string/embedded `id` equals `stored_request_uri`.
/// 2. Fallback: embedded `Join` with `actor` = our actor and
///    `object` = the event uri, and `party.authorized_responder`.
/// 3. Anything else → `false` (drop).
pub fn response_matches(
    stored_request_uri: &str,
    object: &Value,
    event_uri: &str,
    our_actor: &str,
    party: &RsvpParty,
) -> bool {
    // Rule 1 — direct reference.
    if let Some(uri) = object.as_str() {
        if uri == stored_request_uri {
            return true;
        }
    }
    if let Some(id) = object.get("id").and_then(|v| v.as_str()) {
        if id == stored_request_uri {
            return true;
        }
    }
    // Rule 2 — embedded Join echoing our request.
    let is_join = object
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "Join");
    let echoes_us = object
        .get("actor")
        .and_then(|v| v.as_str())
        .is_some_and(|a| a == our_actor);
    let targets_event =
        crate::interactions::object_uri(object.get("object")).is_some_and(|o| o == event_uri);
    is_join && echoes_us && targets_event && party.authorized_responder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::{EventStatus, ParticipantRole, RemoteJoinProtocol};
    use chrono::TimeZone;

    fn event() -> EventRow {
        EventRow {
            status_id: 1,
            start_time: Some(Utc.with_ymd_and_hms(2030, 1, 1, 18, 0, 0).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2030, 1, 1, 20, 0, 0).unwrap()),
            end_time_inferred: false,
            display_end_time: true,
            timezone: Some("UTC".into()),
            utc_offset_seconds: Some(0),
            status: EventStatus::Scheduled,
            previous_start_time: None,
            join_mode: JoinMode::Free,
            external_participation_url: None,
            maximum_attendee_capacity: None,
            participants_count: 0,
            pending_count: 0,
            remote_participants_count: None,
            categories: vec![],
            links: vec![],
            banner_media_id: None,
            flyer_media_id: None,
            invitees: vec![],
            comments_enabled: true,
            organizers_disclosed: true,
            attendees_visibility: crate::events::model::AttendeesVisibility::Public,
            attendees_uri: None,
            remote_join_protocol: None,
            raw_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn env_local() -> RsvpEnv {
        RsvpEnv {
            event_local: true,
            organizer_actor: "https://yonder.example/users/org".into(),
            attributed_to_actor: None,
            required_join_visibility: vec![],
            organizer_actors: vec![],
            our_actor: "https://yonder.example/users/me".into(),
            event_uri: "https://yonder.example/users/org/statuses/1".into(),
            now: Utc.with_ymd_and_hms(2029, 6, 1, 0, 0, 0).unwrap(),
        }
    }

    fn env_remote() -> RsvpEnv {
        RsvpEnv {
            event_local: false,
            organizer_actor: "https://remote.example/users/org".into(),
            attributed_to_actor: Some("https://remote.example/users/group".into()),
            required_join_visibility: vec![],
            organizer_actors: vec!["https://remote.example/users/org".into()],
            our_actor: "https://yonder.example/users/me".into(),
            event_uri: "https://remote.example/users/org/statuses/9".into(),
            now: Utc.with_ymd_and_hms(2029, 6, 1, 0, 0, 0).unwrap(),
        }
    }

    fn party() -> RsvpParty {
        RsvpParty {
            account_id: 42,
            actor_uri: "https://remote.example/users/joiner".into(),
            username: None,
            blocked: false,
            invited: false,
            authorized_responder: false,
        }
    }

    fn join() -> RsvpInput {
        RsvpInput::Join {
            join_uri: Some("https://remote.example/users/joiner#join/1".into()),
            participation_message: Some("see you there".into()),
            tentative: false,
        }
    }

    #[test]
    fn free_join_accepts_and_answers() {
        let out = transition(&event(), None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        let w = out.participation.unwrap();
        assert_eq!(w.state, ParticipationState::Accepted);
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Accept { .. })
        ));
        assert_eq!(out.counters.accepted, 1);
    }

    #[test]
    fn blocked_join_drops() {
        let mut p = party();
        p.blocked = true;
        assert!(transition(&event(), None, join(), &p, &env_local())
            .unwrap()
            .is_none());
    }

    #[test]
    fn capacity_full_rejects() {
        let mut e = event();
        e.maximum_attendee_capacity = Some(2);
        e.participants_count = 2;
        let out = transition(&e, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Reject { .. })
        ));
        assert_eq!(out.counters.accepted, 0);
    }

    #[test]
    fn none_mode_ignores_with_join_id() {
        let mut e = event();
        e.join_mode = JoinMode::None;
        let out = transition(&e, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        match out.outbound {
            Some(OutboundActivity::Ignore { object, .. }) => {
                assert_eq!(object, "https://remote.example/users/joiner#join/1")
            }
            other => panic!("expected Ignore, got {other:?}"),
        }
    }

    #[test]
    fn restricted_join_pends_and_notifies() {
        let mut e = event();
        e.join_mode = JoinMode::Restricted;
        let out = transition(&e, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert_eq!(
            out.participation.unwrap().state,
            ParticipationState::Pending
        );
        assert!(out.notify_organizer);
        assert!(out.outbound.is_none());
        assert_eq!(out.counters.pending, 1);
    }

    #[test]
    fn invite_mode_requires_invitation() {
        let mut e = event();
        e.join_mode = JoinMode::Invite;
        let out = transition(&e, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Reject { .. })
        ));

        let mut p = party();
        p.invited = true;
        let out = transition(&e, None, join(), &p, &env_local())
            .unwrap()
            .unwrap();
        assert_eq!(
            out.participation.unwrap().state,
            ParticipationState::Accepted
        );
    }

    #[test]
    fn cancelled_and_ended_reject() {
        let mut e = event();
        e.status = EventStatus::Cancelled;
        let out = transition(&e, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Reject { .. })
        ));

        let mut e2 = event();
        e2.end_time = Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap());
        let out = transition(&e2, None, join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Reject { .. })
        ));
    }

    #[test]
    fn accepted_rejoin_resends_accept() {
        let existing = EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state: ParticipationState::Accepted,
            protocol: ParticipationProtocol::Join,
            request_activity_uri: Some("j1".into()),
            response_activity_uri: None,
            invite_activity_uri: None,
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let out = transition(&event(), Some(&existing), join(), &party(), &env_local())
            .unwrap()
            .unwrap();
        assert!(matches!(
            out.outbound,
            Some(OutboundActivity::Accept { .. })
        ));
        assert_eq!(out.counters.accepted, 0);
    }

    #[test]
    fn leave_deletes_and_counts_down() {
        let existing = EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state: ParticipationState::Accepted,
            protocol: ParticipationProtocol::Join,
            request_activity_uri: Some("j1".into()),
            response_activity_uri: None,
            invite_activity_uri: None,
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let out = transition(
            &event(),
            Some(&existing),
            RsvpInput::Leave,
            &party(),
            &env_local(),
        )
        .unwrap()
        .unwrap();
        assert!(out.participation.is_none());
        assert_eq!(out.counters.accepted, -1);
        assert!(out.outbound.is_none());
    }

    #[test]
    fn organizer_leave_is_dropped() {
        let existing = EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Organizer,
            state: ParticipationState::Accepted,
            protocol: ParticipationProtocol::Local,
            request_activity_uri: None,
            response_activity_uri: None,
            invite_activity_uri: None,
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(transition(
            &event(),
            Some(&existing),
            RsvpInput::Leave,
            &party(),
            &env_local(),
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn friendica_accept_resolves_invited() {
        let existing = EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state: ParticipationState::Invited,
            protocol: ParticipationProtocol::InviteResponse,
            request_activity_uri: None,
            response_activity_uri: None,
            invite_activity_uri: Some("invite-1".into()),
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let out = transition(
            &event(),
            Some(&existing),
            RsvpInput::AcceptInvite {
                activity_uri: Some("act-1".into()),
            },
            &party(),
            &env_local(),
        )
        .unwrap()
        .unwrap();
        let w = out.participation.unwrap();
        assert_eq!(w.state, ParticipationState::Accepted);
        assert_eq!(w.protocol, ParticipationProtocol::InviteResponse);
        assert_eq!(w.response_activity_uri.as_deref(), Some("act-1"));
    }

    #[test]
    fn remote_invite_response_federates_back() {
        // Our user answering an invite on a *remote* event — the
        // answer must reach the organizer, addressed `to:` them
        // (§5.3), with the Invite's own id as the object when the
        // invitee's row recorded one.
        let invited = |invite_uri: Option<&str>| EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state: ParticipationState::Invited,
            protocol: ParticipationProtocol::InviteResponse,
            request_activity_uri: None,
            response_activity_uri: None,
            invite_activity_uri: invite_uri.map(str::to_string),
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let out = transition(
            &event(),
            Some(&invited(Some("invite-1"))),
            RsvpInput::AcceptInvite { activity_uri: None },
            &party(),
            &env_remote(),
        )
        .unwrap()
        .unwrap();
        match out.outbound {
            Some(OutboundActivity::Accept { object, to, cc }) => {
                assert_eq!(object, "invite-1");
                assert_eq!(to, vec!["https://remote.example/users/org".to_string()]);
                assert_eq!(cc, vec!["https://remote.example/users/group".to_string()]);
            }
            other => panic!("expected Accept, got {other:?}"),
        }
        // No stored invite id → the event URI is the object; a
        // decline answers with `Reject`.
        let out = transition(
            &event(),
            Some(&invited(None)),
            RsvpInput::RejectInvite { activity_uri: None },
            &party(),
            &env_remote(),
        )
        .unwrap()
        .unwrap();
        match out.outbound {
            Some(OutboundActivity::Reject { object, .. }) => {
                assert_eq!(object, "https://remote.example/users/org/statuses/9")
            }
            other => panic!("expected Reject, got {other:?}"),
        }
        // Local events never federate invite responses.
        let out = transition(
            &event(),
            Some(&invited(Some("invite-1"))),
            RsvpInput::AcceptInvite { activity_uri: None },
            &party(),
            &env_local(),
        )
        .unwrap()
        .unwrap();
        assert!(out.outbound.is_none());
    }

    #[test]
    fn pending_join_cannot_self_approve() {
        // A `pending` row on a restricted event is the organizer's
        // decision — the joiner's own `Accept` must not resolve it
        // (review C-2).
        let existing = EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state: ParticipationState::Pending,
            protocol: ParticipationProtocol::Join,
            request_activity_uri: Some("j1".into()),
            response_activity_uri: None,
            invite_activity_uri: None,
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        for input in [
            RsvpInput::AcceptInvite {
                activity_uri: Some("act-1".into()),
            },
            RsvpInput::TentativeAcceptInvite {
                activity_uri: Some("act-1".into()),
            },
            RsvpInput::RejectInvite {
                activity_uri: Some("act-1".into()),
            },
        ] {
            assert!(
                transition(&event(), Some(&existing), input, &party(), &env_local())
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn remote_join_pends_and_builds_join() {
        let mut e = event();
        e.remote_join_protocol = Some(RemoteJoinProtocol::Join);
        let mut p = party();
        p.username = Some("me".into());
        let out = transition(
            &e,
            None,
            RsvpInput::Join {
                join_uri: None,
                participation_message: None,
                tentative: false,
            },
            &p,
            &env_remote(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            out.participation.unwrap().state,
            ParticipationState::Pending
        );
        match out.outbound {
            Some(OutboundActivity::Join { to, cc, .. }) => {
                assert!(to.contains(&"https://remote.example/users/org".to_string()));
                assert!(cc.contains(&"https://remote.example/users/group".to_string()));
            }
            other => panic!("expected Join, got {other:?}"),
        }
    }

    #[test]
    fn remote_response_states_and_ignore_downgrade() {
        let existing = |state| EventParticipationRow {
            id: 1,
            status_id: 1,
            account_id: 42,
            role: ParticipantRole::Participant,
            state,
            protocol: ParticipationProtocol::Join,
            request_activity_uri: Some("join-1".into()),
            response_activity_uri: None,
            invite_activity_uri: None,
            participation_message: None,
            reminded_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let pending = existing(ParticipationState::Pending);
        let e = event();
        let p = party();
        for (kind, want, down) in [
            (
                RemoteResponseKind::Accept,
                ParticipationState::Accepted,
                false,
            ),
            (
                RemoteResponseKind::Reject,
                ParticipationState::Rejected,
                false,
            ),
            (
                RemoteResponseKind::TentativeAccept,
                ParticipationState::Tentative,
                false,
            ),
            (
                RemoteResponseKind::Ignore,
                ParticipationState::Ignored,
                true,
            ),
        ] {
            let out = transition(
                &e,
                Some(&pending),
                RsvpInput::RemoteResponse {
                    kind,
                    activity_uri: Some("resp-1".into()),
                },
                &p,
                &env_remote(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(out.participation.as_ref().unwrap().state, want);
            assert_eq!(out.downgrade_protocol, down);
        }
        // Accepted counter: accept +1, tentative +1, reject/ignore 0.
        let out = transition(
            &e,
            Some(&pending),
            RsvpInput::RemoteResponse {
                kind: RemoteResponseKind::Accept,
                activity_uri: None,
            },
            &p,
            &env_remote(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.counters.accepted, 1);
        assert_eq!(out.counters.pending, -1);
    }

    #[test]
    fn response_matching_rules() {
        let p = RsvpParty {
            authorized_responder: true,
            ..party()
        };
        let stored = "https://yonder.example/users/me#join/abc";
        // Rule 1a: bare string.
        assert!(response_matches(
            stored,
            &json!(stored),
            "https://remote.example/events/1",
            "https://yonder.example/users/me",
            &p
        ));
        // Rule 1b: embedded id.
        assert!(response_matches(
            stored,
            &json!({"id": stored}),
            "https://remote.example/events/1",
            "https://yonder.example/users/me",
            &p
        ));
        // Rule 2: embedded Join echo, authorized responder.
        assert!(response_matches(
            stored,
            &json!({
                "type": "Join",
                "actor": "https://yonder.example/users/me",
                "object": "https://remote.example/events/1",
            }),
            "https://remote.example/events/1",
            "https://yonder.example/users/me",
            &p
        ));
        // Rule 3: unauthorized responder on embedded Join → drop.
        let p2 = RsvpParty {
            authorized_responder: false,
            ..party()
        };
        assert!(!response_matches(
            stored,
            &json!({
                "type": "Join",
                "actor": "https://yonder.example/users/me",
                "object": "https://remote.example/events/1",
            }),
            "https://remote.example/events/1",
            "https://yonder.example/users/me",
            &p2
        ));
        // Unrelated object → drop.
        assert!(!response_matches(
            stored,
            &json!("https://evil.example/spoof"),
            "https://remote.example/events/1",
            "https://yonder.example/users/me",
            &p
        ));
    }

    #[test]
    fn outbound_json_carries_context_and_addressing() {
        let a = OutboundActivity::Accept {
            object: "join-1".into(),
            to: vec!["https://remote.example/users/j".into()],
            cc: vec![],
        };
        let doc = a.to_json("https://yonder.example/users/me");
        assert_eq!(doc["type"], "Accept");
        assert_eq!(doc["object"], "join-1");
        assert!(doc["@context"].is_array());
        assert!(doc["id"].as_str().unwrap().contains("#accept/"));
    }
}
