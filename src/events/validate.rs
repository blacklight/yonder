//! Local event authoring — `events::validate` enforces the §9.1
//! field table for `post_type=event` creates and edits.
//!
//! [`EventParams`] is the raw client payload (the `event` JSON object
//! or the `event[...]` form fields — both deserialize into the same
//! shape); [`ValidatedEvent`] is the normalized result stored in
//! `ValidatedStatus` (and therefore in `scheduled_statuses.params`).
//! [`ValidatedEvent::to_parsed`] adapts it to the remote
//! [`ParsedEvent`] so `remote::sync_event` persists local and remote
//! events through one code path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::EventsConfig;
use crate::events::model::{AttendeesVisibility, EventStatus, JoinMode, LocationKind, TimezoneId};
use crate::events::remote::{self, ParsedEvent, ParsedLocation};
use crate::posts::create::check_media_claimable;
use crate::posts::PostContext;
use crate::{Error, Result};

/// The category set FEP-8a8e recommends — accepted verbatim when
/// `events.allow_custom_categories` is off.
pub const FEP_CATEGORIES: &[&str] = &[
    "ARTS",
    "AUTO_BOAT_AIR",
    "BOOK_CLUBS",
    "BUSINESS",
    "CAUSES",
    "CLIMATE_ENVIRONMENT",
    "COMMUNITY",
    "COMEDY",
    "CRAFTS",
    "CREATIVE_JAM",
    "DIY_MAKER_SPACES",
    "FAMILY_EDUCATION",
    "FASHION_BEAUTY",
    "FESTIVALS",
    "FILM_MEDIA",
    "FOOD_DRINK",
    "GAMES",
    "INCLUSIVE_SPACES",
    "LANGUAGE_CULTURE",
    "LEARNING",
    "LGBTQ",
    "MEETING",
    "MEDITATION_WELLBEING",
    "MOVEMENTS_POLITICS",
    "MUSIC",
    "NETWORKING",
    "OUTDOORS_ADVENTURE",
    "PARTY",
    "PERFORMING_VISUAL_ARTS",
    "PETS",
    "PHOTOGRAPHY",
    "SCIENCE_TECH",
    "SPIRITUALITY_RELIGION_BELIEFS",
    "SPORTS",
    "THEATRE",
    "WORKSHOPS_SKILL_SHARING",
];

/// One `event.locations[]` entry — the §9.1 object shape. Every
/// field optional; `validate` enforces what each `type` needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocationParams {
    /// `place` | `virtual` (also accepts `Place`/`VirtualLocation`).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub address_text: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// The raw `event` request payload — every field optional; missing
/// required fields are validation errors, not parse errors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventParams {
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub timezone: Option<String>,
    pub status: Option<String>,
    pub previous_start_time: Option<String>,
    pub join_mode: Option<String>,
    pub external_participation_url: Option<String>,
    pub maximum_attendee_capacity: Option<i64>,
    pub attendees_visibility: Option<String>,
    pub categories: Vec<String>,
    pub locations: Vec<LocationParams>,
    pub banner_media_id: Option<i64>,
    pub flyer_media_id: Option<i64>,
    pub invitees: Vec<i64>,
    pub display_end_time: Option<bool>,
}

/// A normalized location row (mirrors `event_locations` columns).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedLocation {
    pub kind: String,
    pub name: Option<String>,
    pub url: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub address_text: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// The validated, normalized event payload — stored verbatim in
/// `ValidatedStatus.event` (and therefore in
/// `scheduled_statuses.params` for scheduled posts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatedEvent {
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// Literal UTC offset (seconds) of `start_time`'s own offset —
    /// the display fallback when no IANA zone is known.
    pub utc_offset_seconds: Option<i32>,
    /// Canonical IANA id.
    pub timezone: Option<String>,
    /// `EventStatus` wire value.
    pub status: String,
    pub previous_start_time: Option<DateTime<Utc>>,
    /// `JoinMode` wire value.
    pub join_mode: String,
    pub external_participation_url: Option<String>,
    pub maximum_attendee_capacity: Option<i32>,
    /// `AttendeesVisibility` wire value (`None` = organizers).
    pub attendees_visibility: Option<String>,
    pub categories: Vec<String>,
    pub locations: Vec<ValidatedLocation>,
    pub banner_media_id: Option<i64>,
    pub flyer_media_id: Option<i64>,
    /// Local account ids invited to a `direct` event — persisted as
    /// `status_mentions` rows so `can_see` and the mention fan-out
    /// treat them as participants.
    pub invitees: Vec<i64>,
    pub display_end_time: bool,
    /// `end_time` was inferred (never the case for local authoring —
    /// kept for shape parity with `ParsedEvent`).
    #[serde(default)]
    pub end_time_inferred: bool,
}

impl ValidatedEvent {
    /// Adapt to the remote [`ParsedEvent`] shape so
    /// `remote::sync_event` persists local events through the same
    /// upsert as federated ones. `status_uri` feeds `attendees_uri`;
    /// banner/flyer travel as media ids (parsed images are remote-only).
    pub fn to_parsed(&self, status_uri: &str) -> ParsedEvent {
        ParsedEvent {
            name: None,
            start_time: self.start_time,
            utc_offset_seconds: self.utc_offset_seconds,
            end_time: self.end_time,
            end_time_inferred: self.end_time_inferred,
            display_end_time: self.display_end_time,
            timezone: self.timezone.clone(),
            status: self.status.parse().unwrap_or(EventStatus::Scheduled),
            raw_status: None,
            previous_start_time: self.previous_start_time,
            locations: self
                .locations
                .iter()
                .map(|l| ParsedLocation {
                    kind: l.kind.parse().unwrap_or(LocationKind::Place),
                    uri: None,
                    name: l.name.clone(),
                    url: l.url.clone(),
                    address_text: l.address_text.clone(),
                    street_address: l.street_address.clone(),
                    locality: l.locality.clone(),
                    region: l.region.clone(),
                    postal_code: l.postal_code.clone(),
                    country: l.country.clone(),
                    latitude: l.latitude,
                    longitude: l.longitude,
                })
                .collect(),
            links: Vec::new(),
            banner: None,
            flyer: None,
            categories: self.categories.clone(),
            join_mode: self.join_mode.parse().unwrap_or(JoinMode::Free),
            join_mode_explicit: true,
            external_participation_url: self.external_participation_url.clone(),
            maximum_attendee_capacity: self.maximum_attendee_capacity,
            remote_participants_count: None,
            organizers: Vec::new(),
            organizers_key_present: false,
            organizers_disclosed: true,
            attendees_uri: Some(format!("{status_uri}/attendees")),
            is_online: false,
            comments_enabled: true,
        }
    }
}

/// Rebuild the authoring payload from the stored event row — the
/// merge base for partial-edit callers (the Nicolex `PUT` adapter)
/// that must not reset absent fields to defaults. Times are
/// reformatted as RFC-3339 instants; `previous_start_time` is left
/// `None` so a moved start still auto-fills from the stored row.
/// Returns `None` when the status has no event row.
pub async fn params_from_stored(db: &sqlx::PgPool, status_id: i64) -> Result<Option<EventParams>> {
    let Some(row) = sqlx::query_as::<
        _,
        (
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<String>,
            String,
            String,
            Option<String>,
            Option<i32>,
            Option<String>,
            Vec<String>,
            Option<i64>,
            Option<i64>,
            Vec<i64>,
            bool,
        ),
    >(
        "SELECT start_time, end_time, timezone, status, join_mode, \
             external_participation_url, maximum_attendee_capacity, \
             attendees_visibility, categories, banner_media_id, flyer_media_id, \
             invitees, display_end_time FROM events WHERE status_id = $1",
    )
    .bind(status_id)
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    let (
        start_time,
        end_time,
        timezone,
        status,
        join_mode,
        external_url,
        capacity,
        attendees_visibility,
        categories,
        banner_media_id,
        flyer_media_id,
        invitees,
        display_end_time,
    ) = row;
    let rfc = |t: Option<DateTime<Utc>>| t.map(|d| d.to_rfc3339());
    let locations = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<f64>,
        ),
    >(
        "SELECT kind, name, url, address_text, street_address, locality, \
             region, postal_code, country, latitude, longitude \
         FROM event_locations WHERE status_id = $1 ORDER BY position",
    )
    .bind(status_id)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(
        |(kind, name, url, address_text, street, locality, region, postal, country, lat, lon)| {
            LocationParams {
                kind: Some(kind),
                name,
                url,
                street_address: street,
                locality,
                region,
                postal_code: postal,
                country,
                address_text,
                latitude: lat,
                longitude: lon,
            }
        },
    )
    .collect();
    Ok(Some(EventParams {
        start_time: rfc(start_time),
        end_time: rfc(end_time),
        // Stored timezone is canonical IANA; remote-synced events can
        // lack one — UTC is the honest display fallback for re-validation.
        timezone: Some(timezone.unwrap_or_else(|| "UTC".into())),
        status: Some(status),
        previous_start_time: None,
        join_mode: Some(join_mode),
        external_participation_url: external_url,
        maximum_attendee_capacity: capacity.map(i64::from),
        attendees_visibility,
        categories,
        locations,
        banner_media_id,
        flyer_media_id,
        invitees,
        display_end_time: Some(display_end_time),
    }))
}

/// Deserialize an `event` JSON object; `None`/absent → `None`, a
/// non-object → 422.
pub fn params_from_json(v: Option<&Value>) -> Result<Option<EventParams>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v @ Value::Object(_)) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| Error::Validation(format!("invalid event payload: {e}"))),
        _ => Err(Error::Validation("event must be an object".to_string())),
    }
}

fn opt_str(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_time(raw: &Option<String>, key: &str) -> Result<Option<(DateTime<Utc>, i32)>> {
    let Some(s) = opt_str(raw.as_deref()) else {
        return Ok(None);
    };
    let dt = DateTime::parse_from_rfc3339(&s)
        .map_err(|_| Error::Validation(format!("event.{key} must be an RFC-3339 timestamp")))?;
    Ok(Some((
        dt.with_timezone(&Utc),
        dt.offset().local_minus_utc(),
    )))
}

fn clean_field(v: Option<&str>, max: usize) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(max).collect())
}

fn coord(v: Option<f64>, min: f64, max: f64, key: &str) -> Result<Option<f64>> {
    match v {
        None => Ok(None),
        Some(n) if n.is_finite() && n >= min && n <= max => Ok(Some(n)),
        _ => Err(Error::Validation(format!(
            "event.locations[].{key} out of range"
        ))),
    }
}

fn location_kind(raw: Option<&str>) -> LocationKind {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("virtual") | Some("virtuallocation") | Some("virtual_location") => {
            LocationKind::Virtual
        }
        _ => LocationKind::Place,
    }
}

fn validate_location(l: &LocationParams, index: usize) -> Result<ValidatedLocation> {
    let kind = location_kind(l.kind.as_deref());
    let name = clean_field(l.name.as_deref(), 500);
    let url = clean_field(l.url.as_deref(), 2048);
    if let Some(u) = &url {
        if !remote::url_scheme_ok(u, false) {
            return Err(Error::Validation(format!(
                "event.locations[{index}].url must be an http(s) URL"
            )));
        }
    }
    let latitude = coord(l.latitude, -90.0, 90.0, "latitude")?;
    let longitude = coord(l.longitude, -180.0, 180.0, "longitude")?;
    let address_text = clean_field(l.address_text.as_deref(), 1000);
    let out = ValidatedLocation {
        kind: kind.to_string(),
        name,
        url,
        street_address: clean_field(l.street_address.as_deref(), 500),
        locality: clean_field(l.locality.as_deref(), 500),
        region: clean_field(l.region.as_deref(), 500),
        postal_code: clean_field(l.postal_code.as_deref(), 500),
        country: clean_field(l.country.as_deref(), 500),
        address_text,
        latitude,
        longitude,
    };
    // A place needs at least a name or some address content; a virtual
    // location needs a name or URL — an empty entry is a client bug.
    let has_content = out.name.is_some()
        || out.url.is_some()
        || out.address_text.is_some()
        || out.street_address.is_some()
        || out.locality.is_some();
    if !has_content {
        return Err(Error::Validation(format!(
            "event.locations[{index}] needs a name, url or address"
        )));
    }
    Ok(out)
}

/// Normalize + validate `params` against `[events]` limits, the
/// post's resolved `visibility`/`media_ids`, and (on edit) the stored
/// event row for `previous_start_time` auto-fill.
///
/// `creating` distinguishes the create-only `scheduled`/`tentative`
/// status restriction from edits, which may set any status (cancel,
/// postpone, reschedule, moved-online).
#[allow(clippy::too_many_arguments)]
pub async fn validate(
    ctx: &PostContext,
    account_id: i64,
    cfg: &EventsConfig,
    params: &EventParams,
    visibility: &str,
    media_ids: Option<&[i64]>,
    editing_status_id: Option<i64>,
    creating: bool,
) -> Result<ValidatedEvent> {
    let db = &ctx.db;
    // The stored start + status — needed for `previous_start_time`
    // auto-fill and the implicit `rescheduled` status below.
    let (old_start, old_status): (Option<DateTime<Utc>>, Option<String>) = match editing_status_id {
        Some(status_id) => {
            sqlx::query_as("SELECT start_time, status FROM events WHERE status_id = $1")
                .bind(status_id)
                .fetch_optional(db)
                .await?
                .unwrap_or_default()
        }
        None => (None, None),
    };
    // Status: default scheduled on create; on edit an absent status
    // inherits the stored one (editing the description of a cancelled
    // event must not resurrect it). Create restricts to scheduled /
    // tentative — a new event can't arrive already cancelled.
    let status_param = opt_str(params.status.as_deref());
    let status_raw = status_param
        .clone()
        .or_else(|| old_status.clone())
        .unwrap_or_else(|| "scheduled".into())
        .to_ascii_lowercase();
    let mut status: EventStatus = status_raw.parse().map_err(|_| {
        Error::Validation(format!(
            "event.status must be one of: {}",
            EventStatus::ALL
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    if creating && !matches!(status, EventStatus::Scheduled | EventStatus::Tentative) {
        return Err(Error::Validation(
            "a new event can only be scheduled or tentative".to_string(),
        ));
    }

    // Times.
    let (start, start_offset) = parse_time(&params.start_time, "start_time")?.unzip();
    let end = parse_time(&params.end_time, "end_time")?.map(|(dt, _)| dt);
    if status != EventStatus::Postponed && start.is_none() {
        return Err(Error::Validation(
            "event.start_time is required unless status is postponed".to_string(),
        ));
    }
    let end = match (start, end) {
        (Some(s), Some(e)) if e <= s => {
            return Err(Error::Validation(
                "event.end_time must be after event.start_time".to_string(),
            ));
        }
        (Some(_), e) => e,
        // No start (postponed): an end is meaningless — drop it.
        (None, _) => None,
    };
    let previous =
        parse_time(&params.previous_start_time, "previous_start_time")?.map(|(dt, _)| dt);

    // An edit that moves the start without naming a status is a
    // reschedule — unless the stored event was cancelled/postponed,
    // where an implicit status flip would resurrect or re-schedule
    // it silently.
    let stored_reschedulable = old_status
        .as_deref()
        .is_some_and(|s| matches!(s, "scheduled" | "tentative" | "rescheduled"));
    if !creating
        && status_param.is_none()
        && start.is_some()
        && old_start.is_some()
        && old_start != start
        && stored_reschedulable
    {
        status = EventStatus::Rescheduled;
    }

    // Timezone: required, canonical IANA.
    let timezone = match opt_str(params.timezone.as_deref()) {
        Some(tz) => Some(
            TimezoneId::try_from(tz.as_str())
                .map_err(|e| Error::Validation(format!("event.timezone: {e}")))?
                .as_str()
                .to_string(),
        ),
        None => {
            return Err(Error::Validation(
                "event.timezone is required (IANA id like \"Europe/Rome\")".to_string(),
            ));
        }
    };

    // previous_start_time: explicit value wins (organizer override);
    // otherwise a moved start on edit auto-fills from the stored row.
    let mut previous_start_time = previous;
    if previous_start_time.is_none() && old_start.is_some() && old_start != start {
        previous_start_time = old_start;
    }

    // Join mode + external URL.
    let join_mode_raw = opt_str(params.join_mode.as_deref())
        .unwrap_or_else(|| "free".into())
        .to_ascii_lowercase();
    let join_mode: JoinMode = join_mode_raw.parse().map_err(|_| {
        Error::Validation(format!(
            "event.join_mode must be one of: {}",
            JoinMode::ALL
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let external_url = opt_str(params.external_participation_url.as_deref());
    if let Some(u) = &external_url {
        if u.len() > 2048 || !remote::url_scheme_ok(u, true) {
            return Err(Error::Validation(
                "event.external_participation_url must be an http(s) or mailto URL".to_string(),
            ));
        }
    }
    if join_mode == JoinMode::External && external_url.is_none() {
        return Err(Error::Validation(
            "event.external_participation_url is required for join_mode=external".to_string(),
        ));
    }

    // Capacity.
    let capacity = match params.maximum_attendee_capacity {
        None => None,
        Some(c) if c < 0 => {
            return Err(Error::Validation(
                "event.maximum_attendee_capacity must be >= 0".to_string(),
            ))
        }
        Some(c) => Some(i32::try_from(c).map_err(|_| {
            Error::Validation("event.maximum_attendee_capacity is too large".to_string())
        })?),
    };

    // Attendees visibility.
    let attendees_visibility = match opt_str(params.attendees_visibility.as_deref()) {
        None => None,
        Some(v) => Some(
            v.to_ascii_lowercase()
                .parse::<AttendeesVisibility>()
                .map_err(|_| {
                    Error::Validation(format!(
                        "event.attendees_visibility must be one of: {}",
                        AttendeesVisibility::ALL
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?
                .to_string(),
        ),
    };

    // Categories: normalize like the remote parser, then enforce the
    // FEP list unless custom categories are enabled; count is a hard
    // error (authoring is stricter than lenient ingest).
    let mut categories: Vec<String> = Vec::new();
    for raw in &params.categories {
        let norm = raw.trim().to_uppercase();
        let valid = !norm.is_empty()
            && norm.len() <= 64
            && norm
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            return Err(Error::Validation(format!(
                "event.categories entries must match [A-Z0-9_]{{1,64}} (got {raw:?})"
            )));
        }
        if !cfg.allow_custom_categories && !FEP_CATEGORIES.contains(&norm.as_str()) {
            return Err(Error::Validation(format!(
                "event.categories {norm:?} is not a FEP-8a8e category \
                 (enable events.allow_custom_categories for custom values)"
            )));
        }
        if !categories.contains(&norm) {
            categories.push(norm);
        }
    }
    if categories.len() > cfg.max_categories as usize {
        return Err(Error::Validation(format!(
            "event.categories is capped at {} entries",
            cfg.max_categories
        )));
    }

    // Locations.
    if params.locations.len() > cfg.max_locations as usize {
        return Err(Error::Validation(format!(
            "event.locations is capped at {} entries",
            cfg.max_locations
        )));
    }
    let mut locations = Vec::with_capacity(params.locations.len());
    for (i, l) in params.locations.iter().enumerate() {
        locations.push(validate_location(l, i)?);
    }

    // Banner/flyer must be among the status's media — the same
    // claimable rule as `cover_media_id`: `media_ids` supplied means
    // membership in it; omitted on edit means already attached.
    for media_id in [params.banner_media_id, params.flyer_media_id]
        .into_iter()
        .flatten()
    {
        check_media_claimable(ctx, account_id, media_id, editing_status_id).await?;
        match media_ids {
            Some(ids) if !ids.contains(&media_id) => {
                return Err(Error::Validation(
                    "banner_media_id/flyer_media_id must be one of the status's attachments"
                        .to_string(),
                ));
            }
            Some(_) => {}
            None => match editing_status_id {
                Some(status_id) => {
                    let attached: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM media_attachments \
                         WHERE id = $1 AND status_id = $2)",
                    )
                    .bind(media_id)
                    .bind(status_id)
                    .fetch_one(db)
                    .await?;
                    if !attached {
                        return Err(Error::Validation(
                            "banner_media_id/flyer_media_id must be one of the status's \
                             attachments"
                                .to_string(),
                        ));
                    }
                }
                None => {
                    return Err(Error::Validation(
                        "banner_media_id/flyer_media_id requires the image to be attached \
                         to the status"
                            .to_string(),
                    ))
                }
            },
        }
    }

    // Invitees: direct-only, must be existing local accounts.
    let mut invitees: Vec<i64> = params.invitees.clone();
    invitees.sort_unstable();
    invitees.dedup();
    if !invitees.is_empty() {
        if visibility != "direct" {
            return Err(Error::Validation(
                "event.invitees requires visibility=direct".to_string(),
            ));
        }
        let found: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE id = ANY($1) AND domain IS NULL")
                .bind(&invitees)
                .fetch_all(db)
                .await?;
        if found.len() != invitees.len() {
            return Err(Error::Validation(
                "event.invitees must be existing local accounts".to_string(),
            ));
        }
    }

    Ok(ValidatedEvent {
        start_time: start,
        end_time: end,
        utc_offset_seconds: start_offset,
        timezone,
        status: status.to_string(),
        previous_start_time,
        join_mode: join_mode.to_string(),
        external_participation_url: external_url,
        maximum_attendee_capacity: capacity,
        attendees_visibility,
        categories,
        locations,
        banner_media_id: params.banner_media_id,
        flyer_media_id: params.flyer_media_id,
        invitees,
        display_end_time: params.display_end_time.unwrap_or(true),
        end_time_inferred: false,
    })
}
