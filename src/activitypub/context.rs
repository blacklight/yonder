//! JSON-LD `@context` builders — the pubby [`ap_context`] plus the
//! FEP-8a8e event terms (plan.md §5.1).
//!
//! Hard rules (from the plan):
//! - never redefine AS2 terms (`location`, `Place`, `latitude`,
//!   `longitude`, `startTime`, `endTime`, `name`) — they must stay
//!   ActivityStreams for JSON-LD consumers like Friendica;
//! - never include `https://schema.org` as a context URL (large, and
//!   it would redefine AS terms); the `sc` prefix maps to
//!   `http://schema.org/` inline instead;
//! - pubby's `schema` prefix (`http://schema.org#`) stays untouched —
//!   it backs `PropertyValue`, `sc` backs event terms.

use serde_json::{json, Value};

/// The extra inline terms appended to the `@context` object member —
/// every FEP-8a8e / schema.org term an Event document or RSVP
/// activity may carry.
fn event_terms() -> Value {
    json!({
        "fep8a8e": "https://w3id.org/fep/8a8e/",
        "sc": "http://schema.org/",
        "xsd": "http://www.w3.org/2001/XMLSchema#",
        "eventStatus": { "@id": "fep8a8e:eventStatus", "@type": "@vocab" },
        "EventScheduled": "fep8a8e:EventScheduled",
        "EventTentative": "fep8a8e:EventTentative",
        "EventCancelled": "fep8a8e:EventCancelled",
        "EventPostponed": "fep8a8e:EventPostponed",
        "EventRescheduled": "fep8a8e:EventRescheduled",
        "EventMovedOnline": "fep8a8e:EventMovedOnline",
        "displayEndTime": { "@id": "fep8a8e:displayEndTime", "@type": "xsd:boolean" },
        "previousStartTime": { "@id": "fep8a8e:previousStartTime", "@type": "xsd:dateTime" },
        "timezone": "fep8a8e:timezone",
        "joinMode": "fep8a8e:joinMode",
        "externalParticipationUrl": { "@id": "fep8a8e:externalParticipationUrl", "@type": "@id" },
        "category": { "@id": "fep8a8e:category", "@container": "@list" },
        "isBannerImage": { "@id": "fep8a8e:isBannerImage", "@type": "xsd:boolean" },
        "organizers": { "@id": "fep8a8e:organizers", "@type": "@id" },
        "attendees": { "@id": "fep8a8e:attendees", "@type": "@id" },
        "upcomingEvents": { "@id": "fep8a8e:upcomingEvents", "@type": "@id" },
        "requiredJoinVisibility": { "@id": "fep8a8e:requiredJoinVisibility", "@type": "@id", "@container": "@list" },
        "OrganizersCollection": "fep8a8e:OrganizersCollection",
        "AttendeesCollection": "fep8a8e:AttendeesCollection",
        "VirtualLocation": "sc:VirtualLocation",
        "PostalAddress": "sc:PostalAddress",
        "address": "sc:address",
        "streetAddress": "sc:streetAddress",
        "addressLocality": "sc:addressLocality",
        "addressRegion": "sc:addressRegion",
        "postalCode": "sc:postalCode",
        "addressCountry": "sc:addressCountry",
        "maximumAttendeeCapacity": "sc:maximumAttendeeCapacity",
        "remainingAttendeeCapacity": "sc:remainingAttendeeCapacity",
        "ical": "http://www.w3.org/2002/12/cal/ical#",
        "status": { "@id": "ical:status" },
        "toot": "http://joinmastodon.org/ns#",
        "focalPoint": { "@id": "toot:focalPoint", "@container": "@list" },
        "mz": "https://joinmobilizon.org/ns#",
        "participationMessage": "mz:participationMessage",
        "isOnline": "mz:isOnline"
    })
}

/// `pubby`'s `AP_CONTEXT` with the FEP-8a8e terms merged into its
/// object member — the `@context` for every Event document, event
/// collection and RSVP activity.
pub fn event_context() -> Value {
    let mut ctx = pubby_sdk::model::ap_context();
    let terms = event_terms();
    if let Some(dst) = ctx
        .as_array_mut()
        .and_then(|arr| arr.iter_mut().rev().find(|v| v.is_object()))
        .and_then(|v| v.as_object_mut())
    {
        if let Some(src) = terms.as_object() {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
    ctx
}

/// The compact `eventStatus` token for an `events.status` value.
pub fn event_status_token(status: crate::events::model::EventStatus) -> &'static str {
    use crate::events::model::EventStatus::*;
    match status {
        Scheduled => "EventScheduled",
        Tentative => "EventTentative",
        Cancelled => "EventCancelled",
        Postponed => "EventPostponed",
        Rescheduled => "EventRescheduled",
        MovedOnline => "EventMovedOnline",
    }
}

/// The iCal `status` companion (Mobilizon reads it): cancelled→
/// `CANCELLED`, tentative/postponed→`TENTATIVE`, else `CONFIRMED`.
pub fn ical_status(status: crate::events::model::EventStatus) -> &'static str {
    use crate::events::model::EventStatus::*;
    match status {
        Cancelled => "CANCELLED",
        Tentative | Postponed => "TENTATIVE",
        _ => "CONFIRMED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::EventStatus;

    #[test]
    fn event_context_appends_terms_without_redefining_as2() {
        let ctx = event_context();
        let terms = ctx
            .as_array()
            .unwrap()
            .iter()
            .find_map(|v| v.as_object())
            .expect("object member");
        for key in [
            "eventStatus",
            "displayEndTime",
            "previousStartTime",
            "joinMode",
            "organizers",
            "attendees",
            "VirtualLocation",
            "PostalAddress",
            "focalPoint",
            "mz",
            "ical",
        ] {
            assert!(terms.contains_key(key), "missing term {key}");
        }
        // The hard rule: AS2 terms stay untouched.
        for forbidden in [
            "location",
            "Place",
            "latitude",
            "longitude",
            "startTime",
            "endTime",
            "name",
        ] {
            assert!(!terms.contains_key(forbidden), "redefined {forbidden}");
        }
        // No schema.org context URL anywhere.
        let urls: Vec<&str> = ctx
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(!urls.contains(&"https://schema.org"));
    }

    #[test]
    fn status_tokens_cover_every_variant() {
        for s in EventStatus::ALL {
            assert!(event_status_token(*s).starts_with("Event"));
            assert!(["CONFIRMED", "TENTATIVE", "CANCELLED"].contains(&ical_status(*s)));
        }
    }
}
