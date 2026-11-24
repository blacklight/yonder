//! Row types and enums for the `events` domain tables.
//!
//! Enum columns are `TEXT`; the `text_enum!` macro generates the
//! `FromStr`/`Display` pair plus the `sqlx::Type`/`Decode` impls that
//! let `#[derive(FromRow)]` decode them directly. Unknown values fail
//! loudly rather than silently coercing — remote payloads are
//! normalized to these enums at ingest (`remote.rs`), never stored
//! raw except in `events.raw_status`.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::Error;

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// All variants, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self {
                    $(Self::$variant => $text,)+
                })
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(Error::Validation(format!(
                        "unknown {} value: {other:?}",
                        stringify!($name)
                    ))),
                }
            }
        }

        impl sqlx::Type<sqlx::Postgres> for $name {
            fn type_info() -> sqlx::postgres::PgTypeInfo {
                <String as sqlx::Type<sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            }
        }

        impl<'r> sqlx::Decode<'r, sqlx::Postgres> for $name {
            fn decode(
                value: sqlx::postgres::PgValueRef<'r>,
            ) -> Result<Self, sqlx::error::BoxDynError> {
                let raw = <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
                raw.parse().map_err(|e: Error| e.into())
            }
        }
    };
}

#[allow(clippy::derivable_impls)]
impl Default for EventStatus {
    fn default() -> Self {
        Self::Scheduled
    }
}

#[allow(clippy::derivable_impls)]
impl Default for JoinMode {
    fn default() -> Self {
        Self::Free
    }
}

text_enum! {
    /// `events.status` (FEP-8a8e `eventStatus`, snake-cased).
    EventStatus {
        Scheduled => "scheduled",
        Tentative => "tentative",
        Cancelled => "cancelled",
        Postponed => "postponed",
        Rescheduled => "rescheduled",
        MovedOnline => "moved_online",
    }
}

text_enum! {
    /// `events.join_mode` (FEP-8a8e `joinMode` / Mobilizon).
    JoinMode {
        Free => "free",
        Restricted => "restricted",
        External => "external",
        Invite => "invite",
        None => "none",
    }
}

text_enum! {
    /// `event_participations.role`.
    ParticipantRole {
        Organizer => "organizer",
        Participant => "participant",
    }
}

text_enum! {
    /// `event_participations.state`.
    ParticipationState {
        Pending => "pending",
        Accepted => "accepted",
        Rejected => "rejected",
        Tentative => "tentative",
        Declined => "declined",
        Ignored => "ignored",
        Invited => "invited",
    }
}

text_enum! {
    /// `event_participations.protocol` — which RSVP dialect a row speaks.
    ParticipationProtocol {
        Join => "join",
        InviteResponse => "invite_response",
        Local => "local",
    }
}

text_enum! {
    /// `events.attendees_visibility` — who may read the `attendees`
    /// collection's `orderedItems`.
    AttendeesVisibility {
        Organizers => "organizers",
        Participants => "participants",
        Public => "public",
    }
}

text_enum! {
    /// `events.remote_join_protocol` — how Yonder should RSVP to a
    /// remote event (selected at ingest, see plan.md §7.3).
    RemoteJoinProtocol {
        Join => "join",
        InviteResponse => "invite_response",
        External => "external",
        None => "none",
    }
}

text_enum! {
    /// `event_locations.kind`.
    LocationKind {
        Place => "place",
        Virtual => "virtual",
    }
}

text_enum! {
    /// `event_organizers.kind`.
    OrganizerKind {
        Actor => "actor",
        Link => "link",
        Person => "person",
        Organization => "organization",
    }
}

/// One non-location link on an event — the `events.links` JSONB
/// entries (`attachment[]` `Link` items that are not virtual
/// locations).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EventLink {
    pub name: Option<String>,
    pub href: String,
}

/// The `events` side-table row (one per `object_type = 'Event'`
/// status). `maximum_attendee_capacity` NULL/0 means unlimited —
/// [`EventRow::is_capacity_limited`] is the single place that knows.
#[derive(Debug, Clone, FromRow)]
pub struct EventRow {
    pub status_id: i64,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub end_time_inferred: bool,
    pub display_end_time: bool,
    pub timezone: Option<String>,
    pub utc_offset_seconds: Option<i32>,
    pub status: EventStatus,
    pub previous_start_time: Option<DateTime<Utc>>,
    pub join_mode: JoinMode,
    pub external_participation_url: Option<String>,
    pub maximum_attendee_capacity: Option<i32>,
    pub participants_count: i32,
    pub pending_count: i32,
    pub remote_participants_count: Option<i32>,
    pub categories: Vec<String>,
    /// Non-location `Link` attachments from the remote object —
    /// `[{name, href}]`, persisted at ingest (plan §6).
    #[sqlx(json)]
    pub links: Vec<EventLink>,
    pub banner_media_id: Option<i64>,
    pub flyer_media_id: Option<i64>,
    /// Authoring-side invitee account ids (`0060`) — local events only;
    /// remote syncs never populate it.
    pub invitees: Vec<i64>,
    /// Comments/replies hint from the origin — UI hint only.
    pub comments_enabled: bool,
    pub organizers_disclosed: bool,
    pub attendees_visibility: AttendeesVisibility,
    pub attendees_uri: Option<String>,
    pub remote_join_protocol: Option<RemoteJoinProtocol>,
    /// Original remote status token, kept for debugging.
    pub raw_status: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl EventRow {
    /// `maximum_attendee_capacity` NULL/0 = unlimited.
    pub fn is_capacity_limited(&self) -> bool {
        self.maximum_attendee_capacity.is_some_and(|c| c > 0)
    }

    /// `remainingAttendeeCapacity` semantics: capacity − accepted
    /// participants; `None` when unlimited.
    pub fn remaining_capacity(&self) -> Option<i32> {
        self.maximum_attendee_capacity
            .filter(|&c| c > 0)
            .map(|c| (c - self.participants_count).max(0))
    }
}

/// One `event_locations` row — a physical `Place` or a
/// `VirtualLocation`.
#[derive(Debug, Clone, FromRow)]
pub struct EventLocationRow {
    pub id: i64,
    pub status_id: i64,
    pub position: i16,
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

/// One `event_participations` row — local events track every
/// participant; remote events only local accounts' own rows.
#[derive(Debug, Clone, FromRow)]
pub struct EventParticipationRow {
    pub id: i64,
    pub status_id: i64,
    pub account_id: i64,
    pub role: ParticipantRole,
    pub state: ParticipationState,
    pub protocol: ParticipationProtocol,
    pub request_activity_uri: Option<String>,
    pub response_activity_uri: Option<String>,
    pub invite_activity_uri: Option<String>,
    pub participation_message: Option<String>,
    pub reminded_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl EventParticipationRow {
    pub fn is_organizer(&self) -> bool {
        self.role == ParticipantRole::Organizer
    }
}

/// One `event_organizers` row — disclosed organizers (remote
/// `OrganizersCollection` items, future local co-organizers).
#[derive(Debug, Clone, FromRow)]
pub struct EventOrganizerRow {
    pub status_id: i64,
    pub position: i16,
    pub account_id: Option<i64>,
    pub uri: Option<String>,
    pub name: Option<String>,
    pub kind: OrganizerKind,
}

/// Canonical IANA timezone id (`Europe/Rome`, `Etc/UTC`, `UTC`).
///
/// `chrono_tz::Tz::from_str` alone is too lenient: it also accepts the
/// fixed US abbreviations (`EST`, `MST`, `HST`, `EST5EDT`, …) which are
/// not usable as display zones. This newtype requires a canonical id —
/// a `/`-separated IANA path, or exactly `UTC`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TimezoneId(pub chrono_tz::Tz);

impl TimezoneId {
    /// The IANA id string.
    pub fn as_str(&self) -> &'static str {
        self.0.name()
    }
}

impl TryFrom<&str> for TimezoneId {
    type Error = Error;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        let s = raw.trim();
        let canonical = s == "UTC" || (s.contains('/') && !s.starts_with('/') && !s.ends_with('/'));
        if !canonical {
            return Err(Error::Validation(format!(
                "timezone must be a canonical IANA id like \"Europe/Rome\" (got {raw:?})"
            )));
        }
        s.parse::<chrono_tz::Tz>()
            .map(TimezoneId)
            .map_err(|_| Error::Validation(format!("unknown IANA timezone id: {raw:?}")))
    }
}

impl TryFrom<String> for TimezoneId {
    type Error = Error;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Display for TimezoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timezone_accepts_canonical_ids() {
        assert!(TimezoneId::try_from("Europe/Rome").is_ok());
        assert!(TimezoneId::try_from("Etc/UTC").is_ok());
        assert!(TimezoneId::try_from("UTC").is_ok());
        assert!(TimezoneId::try_from("America/Argentina/Buenos_Aires").is_ok());
        assert_eq!(
            TimezoneId::try_from("Europe/Rome").unwrap().as_str(),
            "Europe/Rome"
        );
    }

    #[test]
    fn timezone_rejects_non_canonical_ids() {
        for bad in ["EST", "+01:00", "", "  ", "EST5EDT", "CET", "not a zone"] {
            assert!(
                TimezoneId::try_from(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn enums_round_trip() {
        for s in EventStatus::ALL {
            let text = s.to_string();
            assert_eq!(text.parse::<EventStatus>().unwrap(), *s);
        }
        for s in JoinMode::ALL {
            assert_eq!(s.to_string().parse::<JoinMode>().unwrap(), *s);
        }
        for s in ParticipationState::ALL {
            assert_eq!(s.to_string().parse::<ParticipationState>().unwrap(), *s);
        }
        assert!("bogus".parse::<EventStatus>().is_err());
    }

    #[test]
    fn capacity_semantics() {
        let mut row = EventRow {
            status_id: 1,
            start_time: None,
            end_time: None,
            end_time_inferred: false,
            display_end_time: true,
            timezone: None,
            utc_offset_seconds: None,
            status: EventStatus::Scheduled,
            previous_start_time: None,
            join_mode: JoinMode::Free,
            links: vec![],
            external_participation_url: None,
            maximum_attendee_capacity: None,
            participants_count: 5,
            pending_count: 0,
            remote_participants_count: None,
            categories: vec![],
            banner_media_id: None,
            flyer_media_id: None,
            invitees: vec![],
            comments_enabled: true,
            organizers_disclosed: true,
            attendees_visibility: AttendeesVisibility::Organizers,
            attendees_uri: None,
            remote_join_protocol: None,
            raw_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(!row.is_capacity_limited());
        assert_eq!(row.remaining_capacity(), None);
        row.maximum_attendee_capacity = Some(0);
        assert!(!row.is_capacity_limited());
        row.maximum_attendee_capacity = Some(10);
        assert!(row.is_capacity_limited());
        assert_eq!(row.remaining_capacity(), Some(5));
    }
}
