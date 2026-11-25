//! Read-path event entity blocks — `yonder.event` (full structured
//! data for Yonder/pl-fe clients) and `pleroma.event` (the Nicolex
//! shape Mastodon-fork clients already understand).
//!
//! [`load_many`] batch-loads everything a page of event statuses
//! needs in a fixed number of queries; [`yonder_event_json`] and
//! [`pleroma_event_json`] are pure builders the status renderer calls
//! per status.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::users::account as account_mapper;
use crate::Result;

use super::model::{
    EventLocationRow, EventOrganizerRow, EventParticipationRow, EventRow, EventStatus, JoinMode,
    LocationKind, ParticipationState, RemoteJoinProtocol,
};

/// Everything the entity needs for one event status, batch-loaded.
pub struct EventBundle {
    pub row: EventRow,
    pub locations: Vec<EventLocationRow>,
    pub organizers: Vec<EventOrganizerRow>,
    /// Resolved `event_organizers.account_id` → account row.
    pub organizer_accounts: HashMap<i64, Arc<account_mapper::AccountRow>>,
    /// The viewer's own `event_participations` row, when any.
    pub participation: Option<EventParticipationRow>,
}

impl EventBundle {
    /// The viewer organizes this event (organizer participation row
    /// or authorship of the status).
    pub fn viewer_is_organizer(&self, viewer: Option<i64>, author_id: i64) -> bool {
        Some(author_id) == viewer
            || self
                .participation
                .as_ref()
                .is_some_and(|p| p.is_organizer())
    }
}

/// Batch-load event data for `status_ids`. One query per child table
/// plus the organizer-account and viewer-participation lookups.
pub async fn load_many(
    db: &PgPool,
    status_ids: &[i64],
    viewer: Option<i64>,
) -> Result<HashMap<i64, EventBundle>> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows_f = sqlx::query_as::<_, EventRow>("SELECT * FROM events WHERE status_id = ANY($1)")
        .bind(status_ids)
        .fetch_all(db);
    let locs_f = sqlx::query_as::<_, EventLocationRow>(
        "SELECT * FROM event_locations WHERE status_id = ANY($1) \
         ORDER BY status_id, position",
    )
    .bind(status_ids)
    .fetch_all(db);
    let orgs_f = sqlx::query_as::<_, EventOrganizerRow>(
        "SELECT * FROM event_organizers WHERE status_id = ANY($1) \
         ORDER BY status_id, position",
    )
    .bind(status_ids)
    .fetch_all(db);
    let part_f = async {
        match viewer {
            Some(v) => {
                sqlx::query_as::<_, EventParticipationRow>(
                    "SELECT * FROM event_participations \
                 WHERE status_id = ANY($1) AND account_id = $2",
                )
                .bind(status_ids)
                .bind(v)
                .fetch_all(db)
                .await
            }
            None => Ok(Vec::new()),
        }
    };
    let (rows, locs, orgs, parts) = tokio::try_join!(rows_f, locs_f, orgs_f, part_f)?;

    let mut bundles: HashMap<i64, EventBundle> = HashMap::new();
    for r in rows {
        bundles.insert(
            r.status_id,
            EventBundle {
                row: r,
                locations: Vec::new(),
                organizers: Vec::new(),
                organizer_accounts: HashMap::new(),
                participation: None,
            },
        );
    }
    for loc in locs {
        if let Some(b) = bundles.get_mut(&loc.status_id) {
            b.locations.push(loc);
        }
    }
    for org in orgs {
        if let Some(b) = bundles.get_mut(&org.status_id) {
            b.organizers.push(org);
        }
    }
    for part in parts {
        if let Some(b) = bundles.get_mut(&part.status_id) {
            b.participation = Some(part);
        }
    }

    // Resolve organizer accounts in one batch.
    let mut account_ids: Vec<i64> = bundles
        .values()
        .flat_map(|b| b.organizers.iter().filter_map(|o| o.account_id))
        .collect();
    account_ids.sort_unstable();
    account_ids.dedup();
    if !account_ids.is_empty() {
        let accounts: HashMap<i64, Arc<account_mapper::AccountRow>> =
            account_mapper::load_many_incl_suspended(db, &account_ids)
                .await?
                .into_iter()
                .map(|a| (a.id, Arc::new(a)))
                .collect();
        for b in bundles.values_mut() {
            for o in &b.organizers {
                if let Some(aid) = o.account_id {
                    if let Some(account) = accounts.get(&aid) {
                        b.organizer_accounts.insert(aid, account.clone());
                    }
                }
            }
        }
    }
    Ok(bundles)
}

// ---------------------------------------------------------------------------
// Entity builders
// ---------------------------------------------------------------------------

/// `rsvp_protocol`: local events speak `local`; remote events use the
/// protocol recorded at ingest (`remote_join_protocol`), falling back
/// to a join_mode-derived guess. The column is only populated by the
/// RSVP section (§7.3), so today the fallback always applies.
fn rsvp_protocol(row: &EventRow, local: bool) -> &'static str {
    if local {
        return "local";
    }
    match row.remote_join_protocol {
        Some(RemoteJoinProtocol::InviteResponse) => "invite_response",
        Some(RemoteJoinProtocol::External) => "external",
        Some(RemoteJoinProtocol::None) => "none",
        Some(RemoteJoinProtocol::Join) => "join",
        None => match row.join_mode {
            JoinMode::External => "external",
            JoinMode::Invite | JoinMode::Restricted => "invite_response",
            _ => "join",
        },
    }
}

/// Why the viewer can't join — `participation.reason` values per
/// plan.md §9.2. `None` means joining is possible.
fn join_block_reason(
    row: &EventRow,
    bundle: &EventBundle,
    viewer: Option<i64>,
    author_id: i64,
    local: bool,
) -> Option<&'static str> {
    if bundle.viewer_is_organizer(viewer, author_id) {
        return Some("own_event");
    }
    if row.status == EventStatus::Cancelled {
        return Some("cancelled");
    }
    if let Some(end) = row.end_time {
        if end <= chrono::Utc::now() {
            return Some("ended");
        }
    }
    match rsvp_protocol(row, local) {
        "external" => return Some("external"),
        "none" => return Some("none"),
        _ => {}
    }
    if row.is_capacity_limited()
        && row.remaining_capacity().unwrap_or(0) <= 0
        && bundle.participation.is_none()
    {
        return Some("full");
    }
    if matches!(row.join_mode, JoinMode::Invite | JoinMode::Restricted)
        && bundle.participation.is_none()
    {
        return Some("invite_only");
    }
    None
}

/// `{state, can_join, reason}` for the viewer.
fn participation_json(
    row: &EventRow,
    bundle: &EventBundle,
    viewer: Option<i64>,
    author_id: i64,
    local: bool,
) -> Value {
    let state = bundle.participation.as_ref().map(|p| p.state.to_string());
    let reason = join_block_reason(row, bundle, viewer, author_id, local);
    let can_join = reason.is_none()
        && !matches!(
            bundle.participation.as_ref().map(|p| &p.state),
            Some(ParticipationState::Accepted | ParticipationState::Pending)
        );
    json!({
        "state": state,
        "can_join": can_join,
        "reason": reason,
    })
}

/// One `event_locations` row → the `yonder.event.locations[]` shape.
fn location_json(loc: &EventLocationRow) -> Value {
    let address = json!({
        "street": loc.street_address,
        "locality": loc.locality,
        "region": loc.region,
        "postal_code": loc.postal_code,
        "country": loc.country,
        "text": loc.address_text,
    });
    let has_address = loc.street_address.is_some()
        || loc.locality.is_some()
        || loc.region.is_some()
        || loc.postal_code.is_some()
        || loc.country.is_some()
        || loc.address_text.is_some();
    json!({
        "type": if loc.kind == LocationKind::Place { "place" } else { "virtual" },
        "uri": loc.uri,
        "name": loc.name,
        "url": loc.url,
        "address": if has_address { address } else { Value::Null },
        "latitude": loc.latitude,
        "longitude": loc.longitude,
    })
}

/// Organizer entries: resolved local accounts render as account
/// entities; remote actors/links degrade to `{name, url}`.
fn organizers_json(
    bundle: &EventBundle,
    base_url: &str,
    config: &crate::config::Config,
) -> Vec<Value> {
    if !bundle.row.organizers_disclosed {
        return Vec::new();
    }
    bundle
        .organizers
        .iter()
        .map(|o| {
            if let Some(aid) = o.account_id {
                if let Some(account) = bundle.organizer_accounts.get(&aid) {
                    return account_mapper::to_account(account, base_url, config);
                }
            }
            json!({
                "name": o.name,
                "url": o.uri,
            })
        })
        .collect()
}

/// `pending_count` is organizer-only — everyone else sees 0.
fn pending_count_visible(bundle: &EventBundle, viewer: Option<i64>, author_id: i64) -> i32 {
    if bundle.viewer_is_organizer(viewer, author_id) {
        bundle.row.pending_count
    } else {
        0
    }
}

/// The `yonder.event` block for one event status — the full §9.2
/// shape event-aware clients consume instead of the read-time
/// preamble. `ics_url` points at the Section-8 route; `media_entity`
/// resolves banner/flyer ids to MediaAttachment entities.
#[allow(clippy::too_many_arguments)]
pub fn yonder_event_json(
    bundle: &EventBundle,
    status_id: i64,
    status_title: Option<&str>,
    description_html: Option<&str>,
    local: bool,
    viewer: Option<i64>,
    author_id: i64,
    base_url: &str,
    config: &crate::config::Config,
    media_entity: &dyn Fn(i64) -> Value,
) -> Value {
    let row = &bundle.row;
    let protocol = rsvp_protocol(row, local);
    // Remote events report their own attendee total; local events
    // count their participation rows. pleroma_event_json prefers the
    // remote count whenever present (Nicolex shape).
    let participants = if local {
        row.participants_count
    } else {
        row.remote_participants_count
            .unwrap_or(row.participants_count)
    };
    json!({
        "name": status_title,
        "content": description_html,
        "start_time": row.start_time.map(|t| t.to_rfc3339()),
        "end_time": row.end_time.map(|t| t.to_rfc3339()),
        "display_end_time": row.display_end_time,
        "end_time_inferred": row.end_time_inferred,
        "timezone": row.timezone,
        "utc_offset": row
            .utc_offset_seconds
            .map(crate::events::summary::utc_offset_label),
        "status": row.status.to_string(),
        "previous_start_time": row.previous_start_time.map(|t| t.to_rfc3339()),
        "join_mode": row.join_mode.to_string(),
        "external_participation_url": row.external_participation_url,
        "rsvp_protocol": protocol,
        "maximum_attendee_capacity": row.maximum_attendee_capacity,
        "participants_count": participants,
        "remaining_capacity": row.remaining_capacity(),
        "pending_count": pending_count_visible(bundle, viewer, author_id),
        "categories": row.categories,
        "locations": bundle.locations.iter().map(location_json).collect::<Vec<_>>(),
        // Non-location `Link` attachments (event homepage, registration
        // page, …), persisted at ingest.
        "links": row
            .links
            .iter()
            .map(|l| json!({"name": l.name, "href": l.href}))
            .collect::<Vec<_>>(),
        "banner": row.banner_media_id.map(media_entity),
        "flyer": row.flyer_media_id.map(media_entity),
        "organizers": organizers_json(bundle, base_url, config),
        "organizers_disclosed": row.organizers_disclosed,
        // The authoring invitee list — organizer-only (it drives the
        // edit composer's picker hydration); string ids for JS safety.
        "invitees": if bundle.viewer_is_organizer(viewer, author_id) {
            json!(row.invitees.iter().map(|id| id.to_string()).collect::<Vec<_>>())
        } else {
            json!([])
        },
        "attendees_visibility": row.attendees_visibility.to_string(),
        "participation": participation_json(row, bundle, viewer, author_id, local),
        "comments_enabled": row.comments_enabled,
        "ics_url": format!("{}/api/v1/yonder/events/{}/ics", base_url.trim_end_matches('/'), status_id),
    })
}

/// The Nicolex `pleroma.event` shape — `join_state` maps
/// pending→`pending`, rejected→`reject`, accepted→`accept`, else null;
/// `location` is the first place. `participation_request_count` is
/// organizer-only, same as `yonder.event.pending_count`.
pub fn pleroma_event_json(
    bundle: &EventBundle,
    status_title: Option<&str>,
    viewer: Option<i64>,
    author_id: i64,
) -> Value {
    let row = &bundle.row;
    let place = bundle
        .locations
        .iter()
        .find(|l| l.kind == LocationKind::Place);
    let join_state = bundle.participation.as_ref().and_then(|p| match p.state {
        ParticipationState::Pending => Some("pending"),
        ParticipationState::Rejected => Some("reject"),
        ParticipationState::Accepted => Some("accept"),
        _ => None,
    });
    json!({
        "name": status_title,
        "start_time": row.start_time.map(|t| t.to_rfc3339()),
        "end_time": row.end_time.map(|t| t.to_rfc3339()),
        "join_mode": row.join_mode.to_string(),
        // Nicolex clients expect the origin's counter when known.
        "participants_count": row.remote_participants_count.unwrap_or(row.participants_count),
        "location": place.map(|l| json!({
            "name": l.name,
            "url": l.url,
            "longitude": l.longitude,
            "latitude": l.latitude,
            "street": l.street_address,
            "postal_code": l.postal_code,
            "locality": l.locality,
            "region": l.region,
            "country": l.country,
        })),
        "join_state": join_state,
        "participation_request_count": pending_count_visible(bundle, viewer, author_id),
    })
}

/// Event-aware Mastodon-API clients render `yonder.event`/
/// `pleroma.event` themselves — the read-time `content` preamble is
/// suppressed for them (owner decision 1b).
pub const EVENT_AWARE_CLIENTS: &[&str] = &["pl-fe", "soapbox", "nicolex"];

/// Whether an OAuth app name belongs to a known event-aware client.
/// Exact match only — a substring check would let any app named e.g.
/// "my-pl-fe-fork" claim event-aware rendering.
pub fn is_event_aware_client(app_name: Option<&str>) -> bool {
    let Some(name) = app_name else { return false };
    EVENT_AWARE_CLIENTS
        .iter()
        .any(|c| name.trim().eq_ignore_ascii_case(c))
}
