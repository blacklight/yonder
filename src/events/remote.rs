//! Remote `Event` object parsing and materialization (FEP-8a8e).
//!
//! [`parse_event`] is a pure `serde_json::Value` → [`ParsedEvent`]
//! parser implementing the normalization table in `plan.md` §6 —
//! lenient about JSON-LD shape (compact keys, no remote context
//! dereferencing) and strict about stored values. [`sync_event`]
//! upserts the `events`/`event_locations`/`event_organizers` rows
//! inside the caller's transaction; [`handle_invite`] materializes
//! Friendica/Hubzilla `Invite{Event}` activities.

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::activitypub::ApCore;
use crate::config::EventsConfig;
use crate::events::model::{
    EventLocationRow, EventRow, EventStatus, JoinMode, LocationKind, OrganizerKind,
};
use crate::Result;

// ---------------------------------------------------------------------------
// Parsed shapes
// ---------------------------------------------------------------------------

/// A `location` entry — physical `Place` or `VirtualLocation`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLocation {
    pub kind: LocationKind,
    pub uri: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub address_text: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// A non-location `Link` attachment (event homepage, registration…)
/// surfaced as `yonder.event.links`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLink {
    pub name: Option<String>,
    pub href: String,
}

/// An image descriptor — banner or flyer. Resolved to a
/// `media_attachments.id` during sync.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedImage {
    pub url: String,
    pub media_type: Option<String>,
    pub name: Option<String>,
}

/// An `organizers`/`organizer` item before account resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOrganizer {
    pub kind: OrganizerKind,
    /// Actor URI (`kind = 'actor'`) or `Link.href`/object `id`.
    pub uri: Option<String>,
    pub name: Option<String>,
}

/// Everything [`parse_event`] extracts from a remote `Event` object.
#[derive(Debug, Clone, Default)]
pub struct ParsedEvent {
    /// `name`/`nameMap` — already HTML-stripped, ≤500 chars.
    pub name: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    /// Literal UTC offset of `startTime` (seconds) — display fallback
    /// when no `timezone` is known.
    pub utc_offset_seconds: Option<i32>,
    pub end_time: Option<DateTime<Utc>>,
    pub end_time_inferred: bool,
    pub display_end_time: bool,
    /// IANA zone id (`chrono_tz` name) or None.
    pub timezone: Option<String>,
    pub status: EventStatus,
    /// Original remote status token, kept for debugging.
    pub raw_status: Option<String>,
    pub previous_start_time: Option<DateTime<Utc>>,
    pub locations: Vec<ParsedLocation>,
    pub links: Vec<ParsedLink>,
    pub banner: Option<ParsedImage>,
    pub flyer: Option<ParsedImage>,
    pub categories: Vec<String>,
    pub join_mode: JoinMode,
    /// Whether a recognized `joinMode` token was present — protocol
    /// selection (Section: RSVP) needs to distinguish "absent" (→
    /// nodeinfo lookup) from an explicit value.
    pub join_mode_explicit: bool,
    pub external_participation_url: Option<String>,
    pub maximum_attendee_capacity: Option<i32>,
    pub remote_participants_count: Option<i32>,
    pub organizers: Vec<ParsedOrganizer>,
    /// `organizers`/`organizer` key was present on the object.
    pub organizers_key_present: bool,
    /// `organizers` was `null`/`totalItems: 0`/empty → deliberately
    /// undisclosed (never falls back to `attributedTo`).
    pub organizers_disclosed: bool,
    pub attendees_uri: Option<String>,
    /// Mobilizon `isOnline`/`mz:isOnline` hint.
    pub is_online: bool,
    /// `commentsEnabled`/`repliesModerationOption` hint.
    pub comments_enabled: bool,
}

// ---------------------------------------------------------------------------
// Small extraction helpers
// ---------------------------------------------------------------------------

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// `{"@id": "…"}` / `{"id": "…"}` / string → the URI string.
fn idish(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => v
            .get("@id")
            .or_else(|| v.get("id"))
            .or_else(|| v.get("href"))
            .and_then(|u| u.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// RFC 3339 instant + its literal UTC offset (seconds). Accepts `Z`.
fn parse_instant(v: Option<&Value>) -> Option<(DateTime<Utc>, i32)> {
    let s = v?.as_str()?;
    let dt = DateTime::parse_from_rfc3339(s).ok()?;
    let offset = dt.offset().local_minus_utc();
    Some((dt.with_timezone(&Utc), offset))
}

/// `name`/`nameMap`: the event language's entry first, else the first
/// key. HTML-stripped, ≤500 chars.
fn localized_name(object: &Value) -> Option<String> {
    let plain = || {
        object
            .get("name")
            .and_then(|v| v.as_str())
            .map(crate::posts::render::html_to_text)
    };
    if let Some(map) = object.get("nameMap").and_then(|v| v.as_object()) {
        let lang = event_language(object);
        let pick = lang
            .as_deref()
            .and_then(|l| map.get(l))
            .and_then(|v| v.as_str())
            .or_else(|| map.values().find_map(|v| v.as_str()));
        if let Some(name) = pick {
            let name = crate::posts::render::html_to_text(name);
            let name: String = name.chars().take(500).collect();
            if !name.trim().is_empty() {
                return Some(name);
            }
        }
    }
    plain()
        .map(|n| n.chars().take(500).collect::<String>())
        .filter(|n| !n.trim().is_empty())
}

/// Best-guess event language: `language`, `inLanguage`, or the first
/// `contentMap` key.
fn event_language(object: &Value) -> Option<String> {
    str_field(object, "language")
        .or_else(|| str_field(object, "inLanguage"))
        .map(str::to_string)
        .or_else(|| {
            object
                .get("contentMap")
                .and_then(|m| m.as_object())
                .and_then(|m| m.keys().next())
                .cloned()
        })
}

/// `type` may be a string, an IRI, a `prefix:` compact name or an
/// array — match the terminal name leniently.
fn type_is(v: &Value, want: &str) -> bool {
    let matches = |t: &str| {
        t == want
            || t.ends_with(&format!(":{want}"))
            || t.ends_with(&format!("/{want}"))
            || t.ends_with(&format!("#{want}"))
    };
    match v.get("type") {
        Some(Value::String(s)) => matches(s),
        Some(Value::Array(a)) => a.iter().filter_map(|t| t.as_str()).any(matches),
        _ => false,
    }
}

fn type_name(v: &Value) -> Option<&str> {
    v.get("type").and_then(|t| match t {
        Value::String(s) => Some(s.as_str()),
        Value::Array(a) => a.iter().find_map(|i| i.as_str()),
        _ => None,
    })
}

/// `displayEndTime`: boolean or `"true"`/`"false"` strings.
fn parse_display_end_time(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `eventStatus`: compact name, FEP/schema IRI, `sc:`/`schema:`
/// prefixed form, or `{"@id": …}`. Fallbacks `status`/`ical:status`
/// (Mobilizon) map CONFIRMED/TENTATIVE/CANCELLED.
fn parse_status(object: &Value) -> (EventStatus, Option<String>) {
    let token = |key: &str| -> Option<String> {
        match object.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(o)) => o
                .get("@id")
                .or_else(|| o.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        }
    };
    let raw = token("eventStatus");
    if let Some(tok) = &raw {
        if let Some(status) = normalize_status_token(tok) {
            return (status, Some(tok.clone()));
        }
        // Unknown eventStatus: keep the token for debugging, default.
        return (EventStatus::Scheduled, Some(tok.clone()));
    }
    for key in ["status", "ical:status"] {
        if let Some(tok) = token(key) {
            let status = match tok.trim().to_ascii_uppercase().as_str() {
                "CONFIRMED" | "CONFIRMED_EVENT" => Some(EventStatus::Scheduled),
                "TENTATIVE" => Some(EventStatus::Tentative),
                "CANCELLED" | "CANCELED" => Some(EventStatus::Cancelled),
                _ => None,
            };
            if let Some(status) = status {
                return (status, Some(tok));
            }
        }
    }
    (EventStatus::Scheduled, None)
}

/// Match a status token to an [`EventStatus`] by its terminal
/// alphanumeric suffix, order-safe (`rescheduled` before `scheduled`).
fn normalize_status_token(token: &str) -> Option<EventStatus> {
    let last = token.rsplit(['#', '/', ':']).next().unwrap_or(token).trim();
    let norm: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let norm = norm.strip_prefix("event").unwrap_or(&norm);
    Some(match norm {
        "movedonline" | "moved_online" => EventStatus::MovedOnline,
        "rescheduled" => EventStatus::Rescheduled,
        "postponed" => EventStatus::Postponed,
        "cancelled" | "canceled" => EventStatus::Cancelled,
        "tentative" => EventStatus::Tentative,
        "scheduled" => EventStatus::Scheduled,
        _ => return None,
    })
}

/// Schemes allowed on stored event URLs (§6 URL-scheme allowlist).
pub(crate) fn url_scheme_ok(url: &str, allow_mailto: bool) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if !lower.contains(':') {
        return false;
    }
    let scheme = lower.split(':').next().unwrap_or("");
    matches!(scheme, "https" | "http") || (allow_mailto && scheme == "mailto")
}

/// Stored-URL check: scheme allowlist **and** the `char_length <=
/// 2048` DB cap — an over-long remote URL must be dropped here, not
/// fail the whole ingest on the CHECK.
fn url_ok(url: &str, allow_mailto: bool) -> bool {
    url.len() <= 2048 && url_scheme_ok(url, allow_mailto)
}

fn clean_str(v: Option<&Value>, max: usize) -> Option<String> {
    v.and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(max).collect())
}

fn coord(v: Option<&Value>, min: f64, max: f64) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (n.is_finite() && n >= min && n <= max).then_some(n)
}

// ---------------------------------------------------------------------------
// Location / link / image / organizer parsing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PostalParts {
    street: Option<String>,
    locality: Option<String>,
    region: Option<String>,
    postal_code: Option<String>,
    country: Option<String>,
}

impl PostalParts {
    fn is_empty(&self) -> bool {
        self.street.is_none()
            && self.locality.is_none()
            && self.region.is_none()
            && self.postal_code.is_none()
            && self.country.is_none()
    }
}

fn postal_address(addr: &Value) -> PostalParts {
    let field = |k: &str| clean_str(addr.get(k), 500);
    PostalParts {
        street: field("streetAddress").or_else(|| field("street_address")),
        locality: field("addressLocality").or_else(|| field("locality")),
        region: field("addressRegion").or_else(|| field("region")),
        postal_code: field("postalCode").or_else(|| field("postal_code")),
        country: field("addressCountry").or_else(|| field("country")),
    }
}

fn parse_location_item(item: &Value) -> ParsedLocation {
    // A bare string is a lenient `Place{name}`.
    if let Some(name) = item.as_str() {
        return ParsedLocation {
            kind: LocationKind::Place,
            uri: None,
            name: Some(name.chars().take(500).collect()),
            url: None,
            address_text: None,
            street_address: None,
            locality: None,
            region: None,
            postal_code: None,
            country: None,
            latitude: None,
            longitude: None,
        };
    }
    let kind = if type_is(item, "VirtualLocation") {
        LocationKind::Virtual
    } else {
        LocationKind::Place
    };
    let name = clean_str(item.get("name"), 500);
    let url = item
        .get("url")
        .and_then(|u| match u {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => u.get("href").and_then(|h| h.as_str()).map(str::to_string),
            _ => None,
        })
        .filter(|u| url_ok(u, false));
    let address = item.get("address");
    let mut loc = ParsedLocation {
        kind,
        uri: idish(item),
        name,
        url,
        address_text: None,
        street_address: None,
        locality: None,
        region: None,
        postal_code: None,
        country: None,
        latitude: coord(
            item.get("latitude").or_else(|| item.get("lat")),
            -90.0,
            90.0,
        ),
        longitude: coord(
            item.get("longitude").or_else(|| item.get("lon")),
            -180.0,
            180.0,
        ),
    };
    match address {
        Some(Value::String(s)) => {
            loc.address_text = Some(s.trim().chars().take(1000).collect());
        }
        Some(a @ Value::Object(_))
            if {
                let is_postal = a.get("type").and_then(|t| t.as_str()).is_some_and(|t| {
                    t == "PostalAddress"
                        || t.ends_with(":PostalAddress")
                        || t.ends_with("/PostalAddress")
                }) || a.get("streetAddress").is_some()
                    || a.get("addressLocality").is_some();
                is_postal
            } =>
        {
            let parts = postal_address(a);
            // Mobilizon sends an all-null PostalAddress with the whole
            // address in `location.name` — never "fix" coordinates,
            // just fall back to the name as display text.
            if parts.is_empty() {
                loc.address_text = loc.name.clone();
            } else {
                loc.street_address = parts.street;
                loc.locality = parts.locality;
                loc.region = parts.region;
                loc.postal_code = parts.postal_code;
                loc.country = parts.country;
            }
        }
        _ => {
            // No usable address object — the place name doubles as
            // the address text (Gancio-style `Place{name, address}`).
            if kind == LocationKind::Place {
                loc.address_text = loc.name.clone();
            }
        }
    }
    loc
}

/// `attachment[]` image-ish check extended for events: a `Document`
/// without `mediaType` but named `Banner` counts as an image (the
/// banner-detection rule), plus the usual mediaType/type shapes.
fn is_image_attachment(att: &Value) -> bool {
    let media_type = str_field(att, "mediaType").unwrap_or("");
    if media_type.starts_with("image/") {
        return true;
    }
    if !media_type.is_empty() {
        return false;
    }
    let is_doc_or_image = matches!(type_name(att), Some("Document") | Some("Image"));
    if !is_doc_or_image {
        return false;
    }
    // `type: Image` without mediaType is an image; `Document` needs
    // the Banner name to qualify.
    type_name(att) == Some("Image")
        || str_field(att, "name").is_some_and(|n| n.trim().eq_ignore_ascii_case("banner"))
}

/// `attachment_kind` extended for events: a `Document`/`Image` named
/// `Banner` without a `mediaType` counts as an image (the banner
/// detection rule in `plan.md` §6).
pub(crate) fn event_attachment_kind(att: &Value) -> Option<&'static str> {
    crate::posts::remote::attachment_kind(att)
        .or_else(|| is_image_attachment(att).then_some("image"))
}

fn attachment_url(att: &Value) -> Option<String> {
    match att.get("url")? {
        Value::String(s) => Some(s.clone()),
        Value::Object(_) => att_url_str(att.get("url")?),
        Value::Array(items) => items.iter().find_map(|u| match u {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => ["href", "url"]
                .iter()
                .find_map(|k| u.get(*k).and_then(|h| h.as_str()))
                .map(str::to_string),
            _ => None,
        }),
        _ => None,
    }
}

fn att_url_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.to_string()),
        Value::Object(_) => ["href", "url"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|h| h.as_str()))
            .map(str::to_string),
        _ => None,
    }
}

fn attachment_name(att: &Value) -> Option<String> {
    att.get("name")
        .or_else(|| att.get("summary"))
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(500).collect())
}

fn image_ref(url: String, media_type: Option<String>, name: Option<String>) -> ParsedImage {
    ParsedImage {
        url,
        media_type,
        name,
    }
}

/// `image` (string / object / array) → the flyer descriptor: largest
/// by pixel area, first wins ties.
fn parse_flyer(object: &Value) -> Option<ParsedImage> {
    fn candidate(v: &Value) -> Option<ParsedImage> {
        match v {
            Value::String(s) => Some(image_ref(s.clone(), None, None)),
            Value::Object(_) => {
                let url = att_url_str(v)?;
                let media_type = str_field(v, "mediaType").map(str::to_string);
                let name = attachment_name(v);
                Some(image_ref(url, media_type, name))
            }
            _ => None,
        }
    }
    let area = |v: &Value| -> i64 {
        let w = v.get("width").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let h = v.get("height").and_then(|x| x.as_f64()).unwrap_or(0.0);
        (w * h) as i64
    };
    match object.get("image")? {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| candidate(v).map(|c| (area(v), c)))
            .max_by_key(|(a, _)| *a)
            .map(|(_, c)| c),
        other => candidate(other),
    }
}

/// Banner: first attachment `isBannerImage`, else first `Document`/
/// `Image` named `Banner`, else first image attachment.
fn parse_banner(object: &Value) -> Option<ParsedImage> {
    let atts = object.get("attachment").and_then(|v| v.as_array())?;
    let to_image = |att: &Value| -> Option<ParsedImage> {
        let url = attachment_url(att)?;
        Some(image_ref(
            url,
            str_field(att, "mediaType").map(str::to_string),
            attachment_name(att),
        ))
    };
    if let Some(att) = atts
        .iter()
        .find(|a| a.get("isBannerImage").and_then(|v| v.as_bool()) == Some(true))
    {
        return to_image(att);
    }
    if let Some(att) = atts.iter().find(|a| {
        is_image_attachment(a)
            && str_field(a, "name").is_some_and(|n| n.trim().eq_ignore_ascii_case("banner"))
    }) {
        return to_image(att);
    }
    atts.iter()
        .find(|a| is_image_attachment(a))
        .and_then(to_image)
}

/// `category`/`categories`/`tag`/`tags` string|array → normalized
/// tokens (upper-case, `[A-Z0-9_]{1,64}`, deduped, ≤ `max`). Entries
/// may be bare strings or `Hashtag`-style objects carrying `name`.
fn parse_categories(object: &Value, max: usize) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    for key in ["category", "categories", "tag", "tags"] {
        match object.get(key) {
            Some(Value::String(s)) => raw.push(s.clone()),
            Some(Value::Array(a)) => {
                for v in a {
                    match v {
                        Value::String(s) => raw.push(s.clone()),
                        Value::Object(_) => {
                            if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                                raw.push(n.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<String> = Vec::new();
    for token in raw {
        if out.len() >= max {
            break;
        }
        let norm = token.trim().to_uppercase();
        let valid = !norm.is_empty()
            && norm.len() <= 64
            && norm
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if valid && !out.contains(&norm) {
            out.push(norm);
        }
    }
    out
}

fn parse_join_mode(object: &Value) -> (JoinMode, bool) {
    match str_field(object, "joinMode").map(|s| s.trim().to_ascii_lowercase()) {
        Some(m) if ["free", "restricted", "external", "invite", "none"].contains(&m.as_str()) => {
            let mode = match m.as_str() {
                "restricted" => JoinMode::Restricted,
                "external" => JoinMode::External,
                "invite" => JoinMode::Invite,
                "none" => JoinMode::None,
                _ => JoinMode::Free,
            };
            (mode, true)
        }
        _ => (JoinMode::Free, false),
    }
}

fn parse_organizers(object: &Value) -> (Vec<ParsedOrganizer>, bool, bool) {
    let Some(value) = object.get("organizers").or_else(|| object.get("organizer")) else {
        // Missing key → the organizer is `attributedTo` (the caller
        // substitutes the author account).
        return (Vec::new(), false, true);
    };
    if value.is_null() {
        return (Vec::new(), true, false);
    }
    // Collection object: `totalItems: 0` or empty items → undisclosed.
    // A bare string (`organizer: "https://…"`) or a single actor
    // object is one organizer, not a collection.
    let items: Vec<Value> = if value.is_string() {
        vec![value.clone()]
    } else if let Some(obj) = value.as_object() {
        if obj.contains_key("items")
            || obj.contains_key("orderedItems")
            || obj.contains_key("first")
            || obj.contains_key("totalItems")
        {
            if obj.get("totalItems").and_then(|v| v.as_i64()) == Some(0) {
                return (Vec::new(), true, false);
            }
            obj.get("items")
                .or_else(|| obj.get("orderedItems"))
                .or_else(|| obj.get("first").and_then(|f| f.get("items")))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        } else {
            vec![value.clone()]
        }
    } else if let Some(arr) = value.as_array() {
        arr.clone()
    } else {
        Vec::new()
    };
    if items.is_empty() {
        // A bare collection URI can't be dereferenced inline — TODO:
        // enqueue a collection fetch (RemoteStatusFetch only handles
        // status objects; a dedicated job or collection-aware fetch is
        // needed). Until then treat like missing (attributedTo
        // fallback) — deferred to a later section.
        if value.is_object() && idish(value).is_some() {
            return (Vec::new(), false, true);
        }
        return (Vec::new(), true, false);
    }
    let mut out = Vec::new();
    for item in &items {
        let parsed = parse_organizer_item(item);
        if let Some(p) = parsed {
            out.push(p);
        }
    }
    if out.is_empty() {
        (Vec::new(), true, false)
    } else {
        (out, true, true)
    }
}

fn parse_organizer_item(item: &Value) -> Option<ParsedOrganizer> {
    match item {
        Value::String(s) => Some(ParsedOrganizer {
            kind: OrganizerKind::Actor,
            uri: Some(s.clone()),
            name: None,
        }),
        Value::Object(_) => {
            let ty = type_name(item).unwrap_or("");
            let name = clean_str(item.get("name"), 500);
            let uri = item
                .get("href")
                .or_else(|| item.get("url"))
                .or_else(|| item.get("id"))
                .and_then(|u| match u {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(_) => u.get("href").and_then(|h| h.as_str()).map(str::to_string),
                    _ => None,
                });
            let kind = if ty.ends_with("Organization") {
                OrganizerKind::Organization
            } else if ty.ends_with("Person") {
                OrganizerKind::Person
            } else if item.get("href").is_some() {
                OrganizerKind::Link
            } else {
                OrganizerKind::Actor
            };
            Some(ParsedOrganizer { kind, uri, name })
        }
        _ => None,
    }
}

fn int_field(v: Option<&Value>) -> Option<i32> {
    match v? {
        Value::Number(n) => n.as_i64().and_then(|i| i32::try_from(i).ok()),
        Value::String(s) => s.trim().parse::<i32>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// parse_event — the pure parser (plan.md §6 normalization table)
// ---------------------------------------------------------------------------

/// Parse a remote `Event` object into [`ParsedEvent`]. `cfg` supplies
/// the `[events]` limits — `default_duration_minutes` (clamped to
/// 1 minute – 1 week so a bad config can't produce `end == start` or
/// overflow the timestamp), `max_locations` and `max_categories`.
pub fn parse_event(object: &Value, cfg: &EventsConfig) -> ParsedEvent {
    let default_duration = Duration::minutes(cfg.default_duration_minutes.clamp(1, 10080) as i64);
    let max_locations = cfg.max_locations as usize;
    let max_categories = cfg.max_categories as usize;
    let mut parsed = ParsedEvent {
        name: localized_name(object),
        status: EventStatus::Scheduled,
        join_mode: JoinMode::Free,
        comments_enabled: true,
        organizers_disclosed: true,
        ..Default::default()
    };

    // Times.
    let (start, offset) = parse_instant(object.get("startTime")).unzip();
    parsed.start_time = start;
    parsed.utc_offset_seconds = offset;
    parsed.previous_start_time = parse_instant(object.get("previousStartTime")).map(|(dt, _)| dt);

    let mut end = parse_instant(object.get("endTime")).map(|(dt, _)| dt);
    let end_present = end.is_some();
    if let (Some(s), Some(e)) = (parsed.start_time, end) {
        if e <= s {
            end = None;
        }
    }
    if end.is_none() && parsed.start_time.is_some() {
        // Missing or inverted endTime → inferred, never displayed.
        parsed.end_time_inferred = true;
        parsed.end_time = parsed.start_time.map(|s| s + default_duration);
        parsed.display_end_time = false;
    } else {
        parsed.end_time = end;
        // Absent + endTime present ⇒ true (FEP MUST).
        parsed.display_end_time =
            parse_display_end_time(object.get("displayEndTime")).unwrap_or(end_present);
    }
    // An end without a start is meaningless — drop it (the CHECK
    // would allow it, nothing downstream wants it).
    if parsed.start_time.is_none() {
        parsed.end_time = None;
        parsed.end_time_inferred = false;
        parsed.display_end_time = false;
    }

    // Timezone: only canonical IANA ids are stored (`TimezoneId`
    // rejects `EST`/`CET`/offsets) — the name is kept verbatim.
    parsed.timezone = str_field(object, "timezone")
        .and_then(|tz| crate::events::model::TimezoneId::try_from(tz).ok())
        .map(|tz| tz.as_str().to_string());

    // Status: an explicit remote token wins over the shape-derived
    // default. A start-less event can only be stored as `postponed` —
    // the `events` CHECK allows NULL `start_time` for nothing else —
    // so coerce rather than fail the whole ingest on the constraint.
    // `previousStartTime` still feeds the "was …" display line.
    let (status, raw) = parse_status(object);
    parsed.status = if parsed.start_time.is_none() {
        EventStatus::Postponed
    } else {
        status
    };
    parsed.raw_status = raw;

    // Locations.
    let is_online = object
        .get("isOnline")
        .or_else(|| object.get("mz:isOnline"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    parsed.is_online = is_online;
    let loc_value = object.get("location");
    let loc_items: Vec<Value> = match loc_value {
        Some(Value::Array(a)) => a.iter().take(max_locations).cloned().collect(),
        Some(v @ (Value::String(_) | Value::Object(_))) => vec![v.clone()],
        _ => Vec::new(),
    };
    for item in &loc_items {
        parsed.locations.push(parse_location_item(item));
    }

    // `attachment[]` Link entries → virtual locations when the event
    // is online or has no other location; otherwise plain links.
    let mut link_attachments: Vec<ParsedLink> = Vec::new();
    if let Some(atts) = object.get("attachment").and_then(|v| v.as_array()) {
        for att in atts {
            if type_is(att, "Link") {
                if let Some(href) = att.get("href").and_then(|h| h.as_str()) {
                    if url_ok(href, false) {
                        link_attachments.push(ParsedLink {
                            name: attachment_name(att),
                            href: href.to_string(),
                        });
                    }
                }
            }
        }
    }
    let has_physical = parsed
        .locations
        .iter()
        .any(|l| l.kind == LocationKind::Place);
    for link in link_attachments {
        if (is_online || !has_physical) && parsed.locations.len() < max_locations {
            parsed.locations.push(ParsedLocation {
                kind: LocationKind::Virtual,
                uri: None,
                name: link.name.clone(),
                url: Some(link.href.clone()),
                address_text: None,
                street_address: None,
                locality: None,
                region: None,
                postal_code: None,
                country: None,
                latitude: None,
                longitude: None,
            });
        } else {
            parsed.links.push(link);
        }
    }

    // Images. A flyer identical to the banner (or to an attachment)
    // is kept — `posts::remote` dedupes media rows by `remote_url`,
    // and `media_id_for_url` still resolves the shared row.
    parsed.banner = parse_banner(object);
    parsed.flyer = parse_flyer(object);

    // Categories.
    parsed.categories = parse_categories(object, max_categories);

    // Join mode.
    let (join_mode, explicit) = parse_join_mode(object);
    parsed.join_mode = join_mode;
    parsed.join_mode_explicit = explicit;
    parsed.external_participation_url = str_field(object, "externalParticipationUrl")
        .filter(|u| url_ok(u, true))
        .map(str::to_string);

    // Capacity.
    parsed.maximum_attendee_capacity =
        int_field(object.get("maximumAttendeeCapacity")).filter(|&c| c >= 0);
    parsed.remote_participants_count = object
        .get("attendees")
        .and_then(|a| a.get("totalItems"))
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .or_else(|| int_field(object.get("participantCount")));
    parsed.attendees_uri = object.get("attendees").and_then(idish).or_else(|| {
        object
            .get("attendees")
            .and_then(|a| a.get("id"))
            .and_then(|i| i.as_str())
            .map(str::to_string)
    });

    // Organizers.
    let (organizers, key_present, disclosed) = parse_organizers(object);
    parsed.organizers = organizers;
    parsed.organizers_key_present = key_present;
    parsed.organizers_disclosed = disclosed;

    // Comments hint.
    let comments_enabled = object
        .get("commentsEnabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
        && !matches!(str_field(object, "repliesModerationOption"), Some("closed"));
    parsed.comments_enabled = comments_enabled;

    parsed
}

// ---------------------------------------------------------------------------
// sync_event — upsert events/event_locations/event_organizers
// ---------------------------------------------------------------------------

/// What a sync did, for post-commit side effects (event_update
/// fan-out, reminder re-arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSyncOutcome {
    pub is_new: bool,
    /// start/end/status/locations changed on an existing event.
    pub material_changed: bool,
    /// `start_time` moved — reminder stamps re-arm.
    pub start_moved: bool,
}

fn loc_signature(l: &EventLocationRow) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}/{:?}",
        l.kind,
        l.uri.as_deref().unwrap_or(""),
        l.name.as_deref().unwrap_or(""),
        l.url.as_deref().unwrap_or(""),
        l.address_text.as_deref().unwrap_or(""),
        l.street_address.as_deref().unwrap_or(""),
        l.locality.as_deref().unwrap_or(""),
        l.region.as_deref().unwrap_or(""),
        l.postal_code.as_deref().unwrap_or(""),
        l.country.as_deref().unwrap_or(""),
        l.latitude,
        l.longitude,
    )
}

fn parsed_loc_signature(l: &ParsedLocation) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}/{:?}",
        l.kind,
        l.uri.as_deref().unwrap_or(""),
        l.name.as_deref().unwrap_or(""),
        l.url.as_deref().unwrap_or(""),
        l.address_text.as_deref().unwrap_or(""),
        l.street_address.as_deref().unwrap_or(""),
        l.locality.as_deref().unwrap_or(""),
        l.region.as_deref().unwrap_or(""),
        l.postal_code.as_deref().unwrap_or(""),
        l.country.as_deref().unwrap_or(""),
        l.latitude,
        l.longitude,
    )
}

/// `(kind, uri, name, account_id)` per `event_organizers` row.
type OrganizerRow = (OrganizerKind, Option<String>, Option<String>, Option<i64>);

/// Upsert the events side tables for `status_id` inside `tx`.
/// `author_account_id` is the status owner (`attributedTo`) — the
/// organizer fallback when the object discloses none.
pub async fn sync_event(
    tx: &mut Transaction<'_, Postgres>,
    status_id: i64,
    parsed: &ParsedEvent,
    author_account_id: i64,
    banner_media_id: Option<i64>,
    flyer_media_id: Option<i64>,
    cfg: &EventsConfig,
) -> Result<EventSyncOutcome> {
    let old = sqlx::query_as::<_, EventRow>(
        "SELECT status_id, start_time, end_time, end_time_inferred, display_end_time, \
             timezone, utc_offset_seconds, status, previous_start_time, join_mode, \
             external_participation_url, maximum_attendee_capacity, participants_count, \
             pending_count, remote_participants_count, categories, links, banner_media_id, \
             flyer_media_id, invitees, comments_enabled, organizers_disclosed, attendees_visibility, \
             attendees_uri, remote_join_protocol, raw_status, created_at, updated_at \
         FROM events WHERE status_id = $1 FOR UPDATE",
    )
    .bind(status_id)
    .fetch_optional(&mut **tx)
    .await?;
    let old_locations: Vec<EventLocationRow> = sqlx::query_as::<_, EventLocationRow>(
        "SELECT id, status_id, position, kind, uri, name, url, address_text, \
             street_address, locality, region, postal_code, country, latitude, longitude \
         FROM event_locations WHERE status_id = $1 ORDER BY position",
    )
    .bind(status_id)
    .fetch_all(&mut **tx)
    .await?;

    let max_locations = cfg.max_locations as usize;
    let categories: Vec<String> = parsed
        .categories
        .iter()
        .take(cfg.max_categories as usize)
        .cloned()
        .collect();
    let links: Vec<crate::events::model::EventLink> = parsed
        .links
        .iter()
        .map(|l| crate::events::model::EventLink {
            name: l.name.clone(),
            href: l.href.clone(),
        })
        .collect();
    sqlx::query(
        "INSERT INTO events (status_id, start_time, end_time, end_time_inferred, \
             display_end_time, timezone, utc_offset_seconds, status, previous_start_time, \
             join_mode, external_participation_url, maximum_attendee_capacity, \
             remote_participants_count, categories, links, banner_media_id, flyer_media_id, \
             comments_enabled, organizers_disclosed, attendees_uri, raw_status) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21) \
         ON CONFLICT (status_id) DO UPDATE SET \
             start_time = EXCLUDED.start_time, end_time = EXCLUDED.end_time, \
             end_time_inferred = EXCLUDED.end_time_inferred, \
             display_end_time = EXCLUDED.display_end_time, \
             timezone = EXCLUDED.timezone, \
             utc_offset_seconds = EXCLUDED.utc_offset_seconds, \
             status = EXCLUDED.status, \
             previous_start_time = EXCLUDED.previous_start_time, \
             join_mode = EXCLUDED.join_mode, \
             external_participation_url = EXCLUDED.external_participation_url, \
             maximum_attendee_capacity = EXCLUDED.maximum_attendee_capacity, \
             remote_participants_count = EXCLUDED.remote_participants_count, \
             categories = EXCLUDED.categories, \
             links = EXCLUDED.links, \
             banner_media_id = EXCLUDED.banner_media_id, \
             flyer_media_id = EXCLUDED.flyer_media_id, \
             comments_enabled = EXCLUDED.comments_enabled, \
             organizers_disclosed = EXCLUDED.organizers_disclosed, \
             attendees_uri = EXCLUDED.attendees_uri, \
             raw_status = EXCLUDED.raw_status, \
             updated_at = now()",
    )
    .bind(status_id)
    .bind(parsed.start_time)
    .bind(parsed.end_time)
    .bind(parsed.end_time_inferred)
    .bind(parsed.display_end_time)
    .bind(&parsed.timezone)
    .bind(parsed.utc_offset_seconds)
    .bind(parsed.status.to_string())
    .bind(parsed.previous_start_time)
    .bind(parsed.join_mode.to_string())
    .bind(&parsed.external_participation_url)
    .bind(parsed.maximum_attendee_capacity)
    .bind(parsed.remote_participants_count)
    .bind(&categories)
    .bind(sqlx::types::Json(&links))
    .bind(banner_media_id)
    .bind(flyer_media_id)
    .bind(parsed.comments_enabled)
    .bind(parsed.organizers_disclosed)
    .bind(&parsed.attendees_uri)
    .bind(&parsed.raw_status)
    .execute(&mut **tx)
    .await?;

    // Locations — full replace, order preserved, capped at config.
    sqlx::query("DELETE FROM event_locations WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut **tx)
        .await?;
    for (pos, loc) in parsed.locations.iter().take(max_locations).enumerate() {
        sqlx::query(
            "INSERT INTO event_locations (status_id, position, kind, uri, name, url, \
                 address_text, street_address, locality, region, postal_code, country, \
                 latitude, longitude) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(status_id)
        .bind(pos as i16)
        .bind(loc.kind.to_string())
        .bind(&loc.uri)
        .bind(&loc.name)
        .bind(&loc.url)
        .bind(&loc.address_text)
        .bind(&loc.street_address)
        .bind(&loc.locality)
        .bind(&loc.region)
        .bind(&loc.postal_code)
        .bind(&loc.country)
        .bind(loc.latitude)
        .bind(loc.longitude)
        .execute(&mut **tx)
        .await?;
    }

    // Organizers — full replace.
    sqlx::query("DELETE FROM event_organizers WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut **tx)
        .await?;
    let organizer_rows: Vec<OrganizerRow> = if !parsed.organizers_disclosed {
        Vec::new()
    } else if parsed.organizers.is_empty() {
        // Missing key → organizer is `attributedTo`.
        vec![(OrganizerKind::Actor, None, None, Some(author_account_id))]
    } else {
        let mut rows = Vec::new();
        for org in &parsed.organizers {
            let account_id = if org.kind == OrganizerKind::Actor {
                match &org.uri {
                    Some(uri) => sqlx::query_scalar::<_, Option<i64>>(
                        "SELECT id FROM accounts WHERE actor_id = $1",
                    )
                    .bind(uri)
                    .fetch_optional(&mut **tx)
                    .await?
                    .flatten(),
                    None => None,
                }
            } else {
                None
            };
            rows.push((org.kind, org.uri.clone(), org.name.clone(), account_id));
        }
        rows
    };
    for (pos, (kind, uri, name, account_id)) in organizer_rows.iter().enumerate() {
        sqlx::query(
            "INSERT INTO event_organizers (status_id, position, account_id, uri, name, kind) \
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(status_id)
        .bind(pos as i16)
        .bind(account_id)
        .bind(uri)
        .bind(name)
        .bind(kind.to_string())
        .execute(&mut **tx)
        .await?;
    }

    // Outcome for post-commit fan-out — compare the stored row against
    // the parsed fields directly (the counters/visibility columns are
    // untouched by remote sync and stay out of the comparison).
    let is_new = old.is_none();
    let material_changed = match &old {
        None => false,
        Some(o) => {
            o.start_time != parsed.start_time
                || o.end_time != parsed.end_time
                || o.status != parsed.status
                || o.links != links
                || loc_signature_vec(&old_locations)
                    != parsed
                        .locations
                        .iter()
                        .take(max_locations)
                        .map(parsed_loc_signature)
                        .collect::<Vec<_>>()
        }
    };
    let start_moved = old
        .as_ref()
        .is_some_and(|o| o.start_time != parsed.start_time);
    if start_moved {
        // A moved start re-arms the reminder sweep.
        sqlx::query(
            "UPDATE event_participations SET reminded_at = NULL, updated_at = now() \
             WHERE status_id = $1 AND reminded_at IS NOT NULL",
        )
        .bind(status_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(EventSyncOutcome {
        is_new,
        material_changed,
        start_moved,
    })
}

fn loc_signature_vec(rows: &[EventLocationRow]) -> Vec<String> {
    rows.iter().map(loc_signature).collect()
}

// ---------------------------------------------------------------------------
// Invite{Event} — Friendica/Hubzilla invitations
// ---------------------------------------------------------------------------

/// `Invite{object: Event}` — materialize the event (embedded or via a
/// deduped fetch), then record `invited` participation rows for each
/// addressed local account. A bare-URI invite whose object hasn't
/// federated yet is parked in `event_pending_invites` and applied by
/// [`drain_pending_invites`] when the event materializes.
///
/// Returns `true` when the invite concerned an event (handled or
/// parked); `false` for non-Event objects so the caller can let them
/// fall through to the default dispatch.
pub async fn handle_invite(
    core: &ApCore,
    activity: &pubby_sdk::Activity,
    raw: &Value,
) -> Result<bool> {
    let object = raw.get("object").cloned().unwrap_or(Value::Null);
    // Addressed actors: the Invite's to/cc — needed both for the
    // immediate path and for parking a bare-URI invite.
    let mut addressees: Vec<String> = Vec::new();
    addressees.extend(crate::posts::remote::str_list(raw.get("to")));
    addressees.extend(crate::posts::remote::str_list(raw.get("cc")));
    addressees.sort();
    addressees.dedup();

    let status_id = match &object {
        Value::String(uri) => {
            match crate::posts::load_by_uri(&core.db, uri).await? {
                Some(s) if s.object_type.as_deref() == Some("Event") => Some(s.id),
                Some(_) => return Ok(false), // a status, but not an event
                None => {
                    // A URI we already know is an actor (not a
                    // status) is a group/actor invite — not ours.
                    let is_actor: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM accounts WHERE actor_id = $1) \
                         OR EXISTS(SELECT 1 FROM actor_cache WHERE actor_id = $1)",
                    )
                    .bind(uri)
                    .fetch_one(&core.db)
                    .await?;
                    if is_actor {
                        return Ok(false);
                    }
                    // Bare-URI invite: only local accounts can receive
                    // `invited` rows — a remote-only invite is not
                    // ours to handle.
                    let local = local_account_addressees(&core.db, &addressees).await?;
                    if local.is_empty() {
                        return Ok(false);
                    }
                    // Park *before* enqueueing the fetch: the worker
                    // may materialize the event before the park lands,
                    // and `drain_pending_invites` would then find
                    // nothing. The park is idempotent, so ordering it
                    // first is safe either way.
                    let parked =
                        park_pending_invites(core, uri, &activity.actor, &activity.id, &local)
                            .await?;
                    if !parked {
                        // Cap hit — this invite can never apply; let it
                        // fall through to the default dispatch.
                        return Ok(false);
                    }
                    // The event may have materialized between the
                    // first lookup and the park — drain now; the
                    // fetch's ingest drains again for later arrivals.
                    match crate::posts::load_by_uri(&core.db, uri).await? {
                        Some(s) if s.object_type.as_deref() == Some("Event") => {
                            if let Err(e) = drain_pending_invites(core, s.id, uri).await {
                                tracing::warn!(status_id = s.id, uri = %uri, error = %e,
                                    "pending invite drain failed; fetch will retry");
                                crate::posts::remote::enqueue_status_fetch_deduped(
                                    &core.redis,
                                    &core.dispatcher,
                                    &core.config.jobs,
                                    uri,
                                )
                                .await?;
                            }
                            return Ok(true);
                        }
                        Some(_) => {
                            // Materialized, but not an event — the
                            // parked rows can never apply; drop them.
                            sqlx::query("DELETE FROM event_pending_invites WHERE event_uri = $1")
                                .bind(uri)
                                .execute(&core.db)
                                .await?;
                            return Ok(false);
                        }
                        None => {}
                    }
                    crate::posts::remote::enqueue_status_fetch_deduped(
                        &core.redis,
                        &core.dispatcher,
                        &core.config.jobs,
                        uri,
                    )
                    .await?;
                    return Ok(true);
                }
            }
        }
        Value::Object(_) if object.get("type").and_then(|t| t.as_str()) == Some("Event") => {
            // Embedded event — materialize through the Create path.
            // This runs before the organizer check below: the event
            // lands under the ownership gate regardless, matching
            // ordinary Create behavior — only the `invited` rows are
            // gated.
            let to = crate::posts::remote::str_list(object.get("to"));
            let cc = crate::posts::remote::str_list(object.get("cc"));
            crate::posts::remote::ingest_embedded(core, &activity.actor, &object, to, cc).await?;
            let uri = object
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            crate::posts::load_by_uri(&core.db, uri)
                .await?
                .filter(|s| s.object_type.as_deref() == Some("Event"))
                .map(|s| s.id)
        }
        _ => None,
    };
    let Some(status_id) = status_id else {
        return Ok(false);
    };
    // Only event statuses carry participations — a non-Event object
    // sharing the URI shape is none of our business.
    let is_event: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE status_id = $1)")
            .bind(status_id)
            .fetch_one(&core.db)
            .await?;
    if !is_event {
        return Ok(false);
    }

    let mut conn = core.db.acquire().await?;
    if !invite_authorized(&mut conn, status_id, &activity.actor).await? {
        tracing::warn!(
            actor = %activity.actor,
            status_id,
            "Invite from non-organizer dropped"
        );
        return Ok(true);
    }
    if addressees.is_empty() {
        return Ok(true);
    }
    let invite_uri = activity.id.clone();
    let invited = write_invited_rows(&mut conn, status_id, &addressees, &invite_uri).await?;
    event_invite_received(core, status_id, invited).await;
    Ok(true)
}

/// Filter invite addressees to local accounts — remote actors never
/// receive `invited` rows, so parking them would only grow the table.
async fn local_account_addressees(db: &sqlx::PgPool, addressees: &[String]) -> Result<Vec<String>> {
    if addressees.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar("SELECT actor_id FROM accounts WHERE domain IS NULL AND actor_id = ANY($1)")
        .bind(addressees)
        .fetch_all(db)
        .await
        .map_err(Into::into)
}

/// Upper bound on parked invites per sender. Parking happens before
/// authorization, so without a cap any remote actor could fill the
/// table with unmaterializable URIs until the retention sweep runs.
const MAX_PENDING_PER_SENDER: i64 = 256;

/// Park a bare-URI invite's addressing until the event materializes.
/// Rows are deduped on (event, invite, addressee); authorization is
/// re-checked at drain time. Returns `false` when the sender hit the
/// parking cap — the invite is then dropped entirely.
async fn park_pending_invites(
    core: &ApCore,
    event_uri: &str,
    actor: &str,
    invite_uri: &str,
    addressees: &[String],
) -> Result<bool> {
    let parked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_pending_invites WHERE actor = $1")
            .bind(actor)
            .fetch_one(&core.db)
            .await?;
    if parked + addressees.len() as i64 > MAX_PENDING_PER_SENDER {
        tracing::warn!(
            actor = %actor,
            event_uri = %event_uri,
            parked,
            cap = MAX_PENDING_PER_SENDER,
            "pending invite cap reached; invite dropped"
        );
        return Ok(false);
    }
    for addressee in addressees {
        sqlx::query(
            "INSERT INTO event_pending_invites \
                 (event_uri, invite_activity_uri, actor, addressee) \
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(event_uri)
        .bind(invite_uri)
        .bind(actor)
        .bind(addressee)
        .execute(&core.db)
        .await?;
    }
    Ok(true)
}

/// Apply parked invites for a just-materialized event. Called
/// post-commit from `posts::remote::upsert` for every Event ingest —
/// a no-op when nothing is pending. Each parked invite re-runs the
/// organizer authorization check against the now-known owner.
pub async fn drain_pending_invites(core: &ApCore, status_id: i64, event_uri: &str) -> Result<()> {
    // Claim and apply inside one transaction: a failure rolls the
    // claim back, so parked rows survive for the next drain (the
    // fetch retry or a later Update). Unauthorized senders are
    // dropped deliberately — their rows are discarded on commit.
    let mut tx = core.db.begin().await?;
    let pending: Vec<(String, String, String)> = sqlx::query_as(
        "DELETE FROM event_pending_invites WHERE event_uri = $1 \
         RETURNING invite_activity_uri, actor, addressee",
    )
    .bind(event_uri)
    .fetch_all(&mut *tx)
    .await?;
    if pending.is_empty() {
        return Ok(());
    }
    // Group by (invite, actor): one authorization check per sender.
    // BTreeMap keeps application order deterministic.
    let mut by_sender: std::collections::BTreeMap<(String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for (invite_uri, actor, addressee) in &pending {
        by_sender
            .entry((invite_uri.clone(), actor.clone()))
            .or_default()
            .push(addressee.clone());
    }
    let mut invited: Vec<i64> = Vec::new();
    for ((invite_uri, actor), addressees) in by_sender {
        if !invite_authorized(&mut tx, status_id, &actor).await? {
            tracing::warn!(actor = %actor, status_id, "Parked invite from non-organizer dropped");
            continue;
        }
        invited.extend(write_invited_rows(&mut tx, status_id, &addressees, &invite_uri).await?);
    }
    tx.commit().await?;
    event_invite_received(core, status_id, invited).await;
    Ok(())
}

/// Drop parked invites older than the retention window — they can
/// only apply if their event federates, which most never do.
/// Called from the remote-activity cleanup sweep.
pub async fn prune_pending_invites(db: &sqlx::PgPool, retention_days: i64) -> Result<u64> {
    let deleted = sqlx::query(
        "DELETE FROM event_pending_invites \
         WHERE created_at < now() - ($1::int * interval '1 day')",
    )
    .bind(retention_days)
    .execute(db)
    .await?
    .rows_affected();
    if deleted > 0 {
        tracing::info!(deleted, "pending event invites pruned");
    }
    Ok(deleted)
}

/// Only the event's author or a disclosed organizer may invite —
/// otherwise any remote actor could mint `invited` participation
/// rows on events they don't control. `event_organizers.uri`
/// covers organizers not yet materialized as local accounts.
async fn invite_authorized(
    db: &mut sqlx::PgConnection,
    status_id: i64,
    actor: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM statuses s JOIN accounts a ON a.id = s.account_id \
             WHERE s.id = $1 AND a.actor_id = $2 \
         ) OR EXISTS( \
             SELECT 1 FROM event_organizers o JOIN accounts a ON a.id = o.account_id \
             WHERE o.status_id = $1 AND a.actor_id = $2 \
         ) OR EXISTS( \
             SELECT 1 FROM event_organizers o \
             WHERE o.status_id = $1 AND o.uri = $2 \
         )",
    )
    .bind(status_id)
    .bind(actor)
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Upsert `invited` participation rows for local addressees — one
/// statement per actor, never clobbering a decided state. Returns the
/// account ids of *newly inserted* rows — re-invites of an existing
/// `invited` row update the stamp silently (no re-notification).
async fn write_invited_rows(
    db: &mut sqlx::PgConnection,
    status_id: i64,
    addressees: &[String],
    invite_uri: &str,
) -> Result<Vec<i64>> {
    let mut invited = Vec::new();
    for actor in addressees {
        let account_id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1 AND domain IS NULL")
                .bind(actor)
                .fetch_optional(&mut *db)
                .await?;
        let Some(account_id) = account_id else {
            continue;
        };
        // One upsert: insert the `invited` row, or refresh the invite
        // stamp on an existing one — never clobber a decided state.
        // `xmax = 0` on RETURNING marks freshly inserted rows.
        let inserted: Option<bool> = sqlx::query_scalar(
            "INSERT INTO event_participations \
                 (status_id, account_id, role, state, protocol, invite_activity_uri) \
             VALUES ($1,$2,'participant','invited','invite_response',$3) \
             ON CONFLICT (status_id, account_id) DO UPDATE SET \
                 invite_activity_uri = EXCLUDED.invite_activity_uri, \
                 updated_at = now() \
             WHERE event_participations.state = 'invited' \
             RETURNING (xmax = 0)",
        )
        .bind(status_id)
        .bind(account_id)
        .bind(invite_uri)
        .fetch_optional(&mut *db)
        .await?;
        if inserted == Some(true) {
            invited.push(account_id);
        }
    }
    Ok(invited)
}

/// A remote `Invite` materialized new local invitee rows — notify
/// exactly those accounts (`pleroma:event_update`), not every
/// existing participant.
pub(crate) async fn event_invite_received(core: &ApCore, status_id: i64, invited: Vec<i64>) {
    if invited.is_empty() {
        return;
    }
    let status = match crate::posts::load(&core.db, status_id).await {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(status_id, error = %e, "event invite status load failed");
            return;
        }
    };
    crate::events::notify::emit_event_update_to(&core.db, &core.dispatcher, &status, &invited)
        .await;
}

/// Post-sync side effects for a remote `Event` Update — notifies
/// local participants (`pleroma:event_update`). Called after the sync
/// transaction commits; failures only log. Reminder re-arming is *not*
/// here: `sync_event` already clears `reminded_at` inside its own
/// transaction, and only when `start_time` actually moved
/// (`EventSyncOutcome::start_moved`).
pub(crate) async fn event_update_fanout(core: &ApCore, status_id: i64) {
    match crate::events::notify::emit_event_update_by_id(&core.db, &core.dispatcher, status_id)
        .await
    {
        Ok(()) => {}
        Err(e) => tracing::warn!(status_id, error = %e, "event_update notify failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "gancio" => include_str!("../../tests/fixtures/events/gancio-event-place.json"),
            "mobilizon-banner" => {
                include_str!("../../tests/fixtures/events/mobilizon-event-banner-nolocation.json")
            }
            "mobilizon-address-name" => include_str!(
                "../../tests/fixtures/events/mobilizon-group-event-address-in-name.json"
            ),
            "mobilizon-place" => {
                include_str!("../../tests/fixtures/events/mobilizon-group-event-place.json")
            }
            "wp-eventbridge" => {
                include_str!("../../tests/fixtures/events/wp-eventbridge-update-event.json")
            }
            _ => panic!("unknown fixture {name}"),
        };
        let v: Value = serde_json::from_str(raw).unwrap();
        v.get("object").cloned().unwrap_or(v)
    }

    fn cfg(minutes: u32) -> EventsConfig {
        EventsConfig {
            default_duration_minutes: minutes,
            ..Default::default()
        }
    }

    fn parse(name: &str) -> ParsedEvent {
        parse_event(&fixture(name), &cfg(120))
    }

    #[test]
    fn gancio_place_event() {
        let p = parse("gancio");
        assert_eq!(p.name.as_deref(), Some("Book Launch: The South Bank"));
        let start = p.start_time.expect("startTime");
        assert_eq!(p.utc_offset_seconds, Some(3600));
        assert!(p.end_time.unwrap() > start);
        assert_eq!(p.status, EventStatus::Scheduled);
        assert_eq!(p.timezone.as_deref(), Some("Europe/Rome"));
        assert_eq!(p.locations.len(), 1);
        let loc = &p.locations[0];
        assert_eq!(loc.kind, LocationKind::Place);
        assert_eq!(loc.name.as_deref(), Some("60-62 Hopton Street"));
        // Coordinates preserved exactly as sent — never "fixed".
        assert_eq!(loc.latitude, Some(51.5083088));
        assert_eq!(loc.longitude, Some(-0.1025268));
        assert!(loc.url.is_some());
    }

    #[test]
    fn mobilizon_banner_without_location() {
        let p = parse("mobilizon-banner");
        assert_eq!(
            p.name.as_deref(),
            Some("Flowers in the Park: Discover the Art of Floristry")
        );
        // `status: CONFIRMED` fallback (no FEP `eventStatus`).
        assert_eq!(p.status, EventStatus::Scheduled);
        assert_eq!(p.raw_status.as_deref(), Some("CONFIRMED"));
        assert_eq!(p.timezone.as_deref(), Some("Etc/UTC"));
        // Document named `Banner` with an image mediaType → banner.
        let banner = p.banner.expect("banner");
        assert!(banner.url.contains("mobilizon.fr/media/"));
        assert_eq!(banner.media_type.as_deref(), Some("image/jpeg"));
        // No physical location → the `Link` attachment is promoted to
        // a virtual location (contract: isOnline or no other location).
        assert_eq!(p.locations.len(), 1);
        assert_eq!(p.locations[0].kind, LocationKind::Virtual);
        assert_eq!(
            p.locations[0].url.as_deref(),
            Some("https://luxflowers.lu/")
        );
        assert!(p.links.is_empty());
        assert_eq!(p.join_mode, JoinMode::Free);
        assert!(p.join_mode_explicit);
    }

    #[test]
    fn mobilizon_address_only_in_name() {
        let p = parse("mobilizon-address-name");
        assert_eq!(p.locations.len(), 1);
        let loc = &p.locations[0];
        assert_eq!(loc.kind, LocationKind::Place);
        // All-null PostalAddress → display falls back to `location.name`.
        assert_eq!(
            loc.address_text.as_deref(),
            Some("28 rue François Arago 76600 le havre")
        );
        assert_eq!(p.timezone.as_deref(), Some("Europe/Paris"));
        // Emoji-prefixed name survives intact.
        assert!(p.name.unwrap().contains("Jam Session"));
    }

    #[test]
    fn mobilizon_place_event() {
        let p = parse("mobilizon-place");
        let loc = &p.locations[0];
        assert_eq!(
            loc.address_text.as_deref(),
            Some("Campus Sciences-U Lyon, 53 Cr Albert Thomas, 69003 Lyon, France")
        );
        assert_eq!(p.status, EventStatus::Scheduled);
    }

    #[test]
    fn wp_eventbridge_flyer() {
        let p = parse("wp-eventbridge");
        assert_eq!(p.name.as_deref(), Some("Federated RSVP"));
        // `image` → flyer; the identical `attachment` entry is deduped
        // downstream by `remote_url`.
        let flyer = p.flyer.expect("flyer");
        assert!(flyer.url.contains("cropped-1a14b2a8"));
        assert_eq!(p.timezone.as_deref(), Some("Europe/Vienna"));
        // No status keys at all → scheduled.
        assert_eq!(p.status, EventStatus::Scheduled);
        assert_eq!(p.raw_status, None);
    }

    #[test]
    fn missing_end_time_is_inferred_and_hidden() {
        let obj = json!({
            "type": "Event",
            "name": "Open ended",
            "startTime": "2026-01-01T10:00:00Z",
        });
        let p = parse_event(&obj, &cfg(90));
        assert!(p.end_time_inferred);
        assert!(!p.display_end_time);
        assert_eq!(
            p.end_time.unwrap(),
            p.start_time.unwrap() + Duration::minutes(90)
        );
    }

    #[test]
    fn event_status_iri_forms() {
        for (token, want) in [
            ("EventScheduled", EventStatus::Scheduled),
            (
                "https://www.w3.org/ns/activitystreams#EventScheduled",
                EventStatus::Scheduled,
            ),
            ("as:EventTentative", EventStatus::Tentative),
            ("EventCancelled", EventStatus::Cancelled),
            ("EventPostponed", EventStatus::Postponed),
            ("EventRescheduled", EventStatus::Rescheduled),
            ("EventMovedOnline", EventStatus::MovedOnline),
        ] {
            let obj = json!({
                "type": "Event",
                "eventStatus": token,
                "startTime": "2026-01-01T10:00:00Z",
            });
            assert_eq!(parse_event(&obj, &cfg(60)).status, want, "{token}");
        }
        // `{"@id": …}` object form.
        let obj = json!({
            "type": "Event",
            "eventStatus": {"@id": "https://www.w3.org/ns/activitystreams#EventCancelled"},
            "startTime": "2026-01-01T10:00:00Z",
        });
        assert_eq!(parse_event(&obj, &cfg(60)).status, EventStatus::Cancelled);
    }

    #[test]
    fn mobilizon_status_fallback() {
        for (token, want) in [
            ("CONFIRMED", EventStatus::Scheduled),
            ("TENTATIVE", EventStatus::Tentative),
            ("CANCELLED", EventStatus::Cancelled),
        ] {
            let obj = json!({
                "type": "Event",
                "status": token,
                "startTime": "2026-01-01T10:00:00Z",
            });
            assert_eq!(parse_event(&obj, &cfg(60)).status, want, "{token}");
        }
    }

    #[test]
    fn name_map_prefers_event_language() {
        let obj = json!({
            "type": "Event",
            "name": "Fallback name",
            "nameMap": {"en": "English name", "fr": "Nom français"},
            "inLanguage": "fr",
        });
        assert_eq!(
            parse_event(&obj, &cfg(60)).name.as_deref(),
            Some("Nom français")
        );
    }

    #[test]
    fn categories_string_or_array() {
        let obj = json!({"type": "Event", "tag": "Music"});
        assert_eq!(parse_event(&obj, &cfg(60)).categories, vec!["MUSIC"]);
        let obj = json!({"type": "Event", "tag": ["music", {"type": "Hashtag", "name": "jazz"}]});
        assert_eq!(
            parse_event(&obj, &cfg(60)).categories,
            vec!["MUSIC", "JAZZ"]
        );
    }

    #[test]
    fn organizer_alias_and_undisclosed() {
        let obj = json!({
            "type": "Event",
            "organizer": "https://example.org/organizer",
        });
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.organizers.len(), 1);
        assert_eq!(
            p.organizers[0].uri.as_deref(),
            Some("https://example.org/organizer")
        );
        // Explicitly undisclosed organizers never fall back.
        let obj = json!({"type": "Event", "organizers": null, "attributedTo": "https://x/"});
        let p = parse_event(&obj, &cfg(60));
        assert!(p.organizers.is_empty());
        assert!(!p.organizers_disclosed);
    }

    #[test]
    fn missing_start_time_becomes_postponed() {
        // The `events` CHECK allows NULL start_time only for
        // `postponed` — the parser must coerce rather than fail the
        // whole ingest.
        let obj = json!({"type": "Event", "name": "No start"});
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.status, EventStatus::Postponed);
        assert!(p.start_time.is_none());
        assert!(p.end_time.is_none());
        // An explicit cancelled status is still coerced — the row
        // can't store it without a start.
        let obj = json!({
            "type": "Event",
            "name": "Cancelled, no start",
            "eventStatus": "EventCancelled",
            "endTime": "2026-01-02T10:00:00Z",
        });
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.status, EventStatus::Postponed);
        assert_eq!(p.raw_status.as_deref(), Some("EventCancelled"));
        assert!(p.end_time.is_none());
        // previousStartTime keeps feeding the "was …" line.
        let obj = json!({
            "type": "Event",
            "name": "Moved",
            "previousStartTime": "2026-01-01T10:00:00Z",
        });
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.status, EventStatus::Postponed);
        assert!(p.previous_start_time.is_some());
    }

    #[test]
    fn config_limits_apply() {
        // max_categories / max_locations come from `[events]`, not
        // hard-coded values.
        let mut c = cfg(60);
        c.max_categories = 1;
        c.max_locations = 1;
        let obj = json!({
            "type": "Event",
            "name": "Capped",
            "category": ["A", "B", "C"],
            "location": [
                {"type": "Place", "name": "One"},
                {"type": "Place", "name": "Two"},
            ],
        });
        let p = parse_event(&obj, &c);
        assert_eq!(p.categories, vec!["A"]);
        assert_eq!(p.locations.len(), 1);
        // A zero duration can't produce end == start (CHECK).
        let mut c = cfg(60);
        c.default_duration_minutes = 0;
        let obj = json!({
            "type": "Event",
            "name": "Zero duration",
            "startTime": "2026-01-01T10:00:00Z",
        });
        let p = parse_event(&obj, &c);
        assert!(p.end_time.unwrap() > p.start_time.unwrap());
    }

    #[test]
    fn noncanonical_timezone_is_dropped() {
        // `TimezoneId` accepts only canonical IANA ids — abbreviations
        // and fixed offsets must not reach the `events` table.
        for tz in ["EST", "CET", "EST5EDT", "+01:00", "UTC"] {
            let obj = json!({
                "type": "Event",
                "timezone": tz,
                "startTime": "2026-01-01T10:00:00Z",
            });
            let p = parse_event(&obj, &cfg(60));
            if tz == "UTC" {
                assert_eq!(p.timezone.as_deref(), Some("UTC"), "{tz}");
            } else {
                assert_eq!(p.timezone, None, "{tz} should be rejected");
            }
        }
    }

    #[test]
    fn oversized_urls_and_runaway_counts_are_dropped() {
        // URL fields carry a 2048-char DB CHECK; counts must fit i32.
        // Oversized values degrade to absent rather than failing the
        // whole ingest.
        let long = format!("https://example.org/{}", "a".repeat(3000));
        let obj = json!({
            "type": "Event",
            "startTime": "2026-01-01T10:00:00Z",
            "externalParticipationUrl": long,
            "location": [{"type": "VirtualLocation", "url": long}],
            "maximumAttendeeCapacity": 4_294_967_297u64,
            "attendees": {"totalItems": 5_000_000_000u64},
        });
        let p = parse_event(&obj, &cfg(60));
        assert!(p.external_participation_url.is_none());
        assert_eq!(p.locations.len(), 1);
        assert!(p.locations[0].url.is_none());
        assert!(p.maximum_attendee_capacity.is_none());
        assert!(p.remote_participants_count.is_none());
        // In-range values still parse.
        let obj = json!({
            "type": "Event",
            "startTime": "2026-01-01T10:00:00Z",
            "maximumAttendeeCapacity": 100,
            "attendees": {"totalItems": 42},
        });
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.maximum_attendee_capacity, Some(100));
        assert_eq!(p.remote_participants_count, Some(42));
    }

    #[test]
    fn unknown_type_fields_degrade_safely() {
        // No FEP context, weird types, unknown status token — must not panic.
        let obj = json!({
            "type": ["Event", "Thing"],
            "eventStatus": "https://example.org/ns#WeirdStatus",
            "timezone": "Not/AZone",
            "location": {"type": "VirtualLocation", "url": "https://meet.example.org/"},
            "startTime": "2026-01-01T10:00:00Z",
        });
        let p = parse_event(&obj, &cfg(60));
        assert_eq!(p.status, EventStatus::Scheduled);
        assert_eq!(p.timezone, None);
        assert_eq!(p.locations[0].kind, LocationKind::Virtual);
    }
}
