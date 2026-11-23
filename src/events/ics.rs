//! RFC 5545 (iCalendar) export for events — hand-rolled writer.
//!
//! One `VEVENT` per event inside a `VCALENDAR`. `DTSTART`/`DTEND`
//! use `TZID=<iana>` when the event carries a timezone (VTIMEZONE-less
//! TZID — accepted by Google Calendar, Apple Calendar and Thunderbird;
//! documented as acceptable in the implementation contract), else a
//! literal UTC offset, else `Z`. `DTEND` is emitted only when
//! `display_end_time` is true — the inferred end of an open-ended
//! event is never shown.

use chrono::{DateTime, TimeZone, Utc};

use crate::{
    events::model::{EventLocationRow, EventRow, EventStatus, LocationKind},
    posts::StatusRow,
    Result,
};

/// Escape a TEXT value per RFC 5545 §3.3.11.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\r' => {}
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Fold one content line at 75 octets (RFC 5545 §3.1): continuation
/// lines start with a space, which counts toward the limit. Cutting
/// never splits a UTF-8 sequence.
fn fold_line(line: &str, out: &mut String) {
    let mut start = 0usize;
    let bytes = line.as_bytes();
    let mut limit = 75usize;
    loop {
        if bytes.len() - start <= limit {
            out.push_str(&line[start..]);
            out.push_str("\r\n");
            return;
        }
        let mut end = start + limit;
        // Back off to a char boundary.
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n ");
        start = end;
        limit = 74; // continuation lines lose one byte to the space
    }
}

/// `dt` rendered for DTSTART/DTEND. Returns `(tzid, value)` — `tzid`
/// is `Some` only for the `TZID=` parameter form.
fn ics_instant(row: &EventRow, dt: DateTime<Utc>) -> (Option<String>, String) {
    if let Some(tz_name) = row.timezone.as_deref() {
        if let Ok(tz) = crate::events::model::TimezoneId::try_from(tz_name) {
            let local = tz.0.from_utc_datetime(&dt.naive_utc());
            return (
                Some(tz_name.to_string()),
                local.format("%Y%m%dT%H%M%S").to_string(),
            );
        }
    }
    // No valid TZID → the UTC instant (`Z`). RFC 5545 accepts only
    // `Z`, floating local time or `TZID=` — a bare `+0100` suffix is
    // invalid, so `utc_offset_seconds` is not used here.
    (None, dt.format("%Y%m%dT%H%M%SZ").to_string())
}

/// RFC 5545 `STATUS` — the same mapping the AP `ical:status` term
/// uses, so both exports agree.
fn ics_status(status: EventStatus) -> &'static str {
    crate::activitypub::context::ical_status(status)
}

/// Flatten one location to a single `LOCATION`/`GEO` candidate.
fn location_text(loc: &EventLocationRow) -> String {
    match loc.kind {
        LocationKind::Place => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(name) = &loc.name {
                parts.push(name.clone());
            }
            if let Some(text) = &loc.address_text {
                parts.push(text.clone());
            } else {
                for piece in [
                    &loc.street_address,
                    &loc.locality,
                    &loc.region,
                    &loc.postal_code,
                    &loc.country,
                ]
                .into_iter()
                .flatten()
                {
                    parts.push(piece.clone());
                }
            }
            parts.join(", ")
        }
        LocationKind::Virtual => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(name) = &loc.name {
                parts.push(name.clone());
            }
            if let Some(url) = &loc.url {
                parts.push(url.clone());
            }
            parts.join(" — ")
        }
    }
}

/// iCalendar `PARTSTAT` for a participation state — the personalized
/// calendar feed (Section 18) renders the viewer's RSVP this way.
/// `None` for states that don't map (pending/rejected/ignored —
/// those events simply don't appear in the feed).
pub fn partstat(state: &str) -> Option<&'static str> {
    match state {
        "invited" => Some("NEEDS-ACTION"),
        "accepted" => Some("ACCEPTED"),
        "tentative" => Some("TENTATIVE"),
        "declined" => Some("DECLINED"),
        _ => None,
    }
}

/// Optional `ORGANIZER`/`ATTENDEE` lines for the personalized feed —
/// `None` fields emit nothing (public feeds stay attendee-free).
#[derive(Debug, Default)]
pub struct VEventParty<'a> {
    /// `(cn, uri)` → `ORGANIZER;CN=…:{uri}`.
    pub organizer: Option<(&'a str, &'a str)>,
    /// `(partstat, cn, uri)` → `ATTENDEE;PARTSTAT=…;CN=…:{uri}`.
    pub attendee: Option<(&'a str, &'a str, &'a str)>,
}

/// One `VEVENT` for `status` + its event bundle data. Returns the
/// folded block (ending in CRLF) or `None` when the event has no
/// start time (postponed events carry no calendar slot).
fn vevent(
    status: &StatusRow,
    row: &EventRow,
    locations: &[EventLocationRow],
    party: &VEventParty<'_>,
    out: &mut String,
) -> Result<()> {
    let Some(start) = row.start_time else {
        return Ok(());
    };
    let mut lines: Vec<String> = Vec::new();
    let uid = status
        .uri
        .clone()
        .or_else(|| status.url.clone())
        .unwrap_or_else(|| format!("status-{}", status.id));
    lines.push(format!("UID:{}", escape_text(&uid)));
    let stamp = status.updated_at;
    lines.push(format!("DTSTAMP:{}", stamp.format("%Y%m%dT%H%M%SZ")));
    let (tzid, value) = ics_instant(row, start);
    match tzid {
        Some(tz) => lines.push(format!("DTSTART;TZID={}:{}", tz, value)),
        None => lines.push(format!("DTSTART:{}", value)),
    }
    // `display_end_time = false` → inferred/hidden end: no DTEND.
    if row.display_end_time {
        if let Some(end) = row.end_time {
            let (tzid, value) = ics_instant(row, end);
            match tzid {
                Some(tz) => lines.push(format!("DTEND;TZID={}:{}", tz, value)),
                None => lines.push(format!("DTEND:{}", value)),
            }
        }
    }
    lines.push(format!("STATUS:{}", ics_status(row.status)));
    let name = status
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| status.content.lines().next().unwrap_or("").to_string());
    if !name.is_empty() {
        lines.push(format!("SUMMARY:{}", escape_text(&name)));
    }
    if !status.content.trim().is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape_text(&status.content)));
    }
    if let Some(html) = status.content_html.as_deref() {
        if !html.trim().is_empty() {
            lines.push(format!(
                "X-ALT-DESC;FMTTYPE=text/html:{}",
                escape_text(html)
            ));
        }
    }
    let location_texts: Vec<String> = locations
        .iter()
        .map(location_text)
        .filter(|t| !t.is_empty())
        .collect();
    if !location_texts.is_empty() {
        lines.push(format!(
            "LOCATION:{}",
            escape_text(&location_texts.join("; "))
        ));
    }
    // First place location with coordinates wins.
    if let Some((lat, lon)) = locations.iter().find_map(|l| {
        l.latitude
            .zip(l.longitude)
            .filter(|_| l.kind == LocationKind::Place)
    }) {
        lines.push(format!("GEO:{lat};{lon}"));
    }
    let url = status.url.clone().or_else(|| status.uri.clone());
    if let Some(url) = url {
        lines.push(format!("URL:{}", escape_text(&url)));
    }
    if let Some((cn, uri)) = party.organizer {
        lines.push(format!(
            "ORGANIZER;CN={}:{}",
            escape_text(cn),
            escape_text(uri)
        ));
    }
    if let Some((partstat, cn, uri)) = party.attendee {
        lines.push(format!(
            "ATTENDEE;PARTSTAT={partstat};CN={}:{}",
            escape_text(cn),
            escape_text(uri)
        ));
    }
    for line in lines {
        fold_line(&line, out);
    }
    Ok(())
}

fn calendar_open(name: &str) -> String {
    let mut out = String::new();
    fold_line("BEGIN:VCALENDAR", &mut out);
    fold_line("VERSION:2.0", &mut out);
    fold_line("PRODID:-//Yonder//Events//EN", &mut out);
    fold_line("CALSCALE:GREGORIAN", &mut out);
    if !name.is_empty() {
        fold_line(&format!("X-WR-CALNAME:{}", escape_text(name)), &mut out);
    }
    out
}

fn calendar_close(out: &mut String) {
    fold_line("END:VCALENDAR", out);
}

// --- Public surface for the personalized feed (`events::calendar`) ---

/// `calendar_open` for callers outside this module.
pub fn calendar_open_pub(name: &str, out: &mut String) {
    out.push_str(&calendar_open(name));
}

/// `calendar_close` for callers outside this module.
pub fn calendar_close_pub(out: &mut String) {
    calendar_close(out);
}

/// `vevent` (with party lines) for callers outside this module —
/// returns `false` when the event has no start (postponed).
pub fn vevent_pub(
    status: &StatusRow,
    row: &EventRow,
    locations: &[EventLocationRow],
    party: &VEventParty<'_>,
    out: &mut String,
) -> bool {
    if row.start_time.is_none() {
        return false;
    }
    fold_line("BEGIN:VEVENT", out);
    let _ = vevent(status, row, locations, party, out);
    fold_line("END:VEVENT", out);
    true
}

/// ICS for one event — `None` when the event has no start time
/// (postponed events carry no calendar slot), so the route can 404.
pub fn event_ics(
    status: &StatusRow,
    row: &EventRow,
    locations: &[EventLocationRow],
) -> Result<Option<String>> {
    if row.start_time.is_none() {
        return Ok(None);
    }
    let mut out = calendar_open("");
    fold_line("BEGIN:VEVENT", &mut out);
    vevent(status, row, locations, &VEventParty::default(), &mut out)?;
    fold_line("END:VEVENT", &mut out);
    calendar_close(&mut out);
    Ok(Some(out))
}

/// One upcoming event for the per-user feed.
pub struct FeedEvent<'a> {
    pub status: &'a StatusRow,
    pub row: &'a EventRow,
    pub locations: &'a [EventLocationRow],
}

/// `VCALENDAR` of a user's upcoming public events.
pub fn user_ics(events: &[FeedEvent<'_>]) -> String {
    let mut out = calendar_open("Yonder events");
    for e in events {
        // A postponed/no-start event contributes nothing — skip it
        // *before* writing VEVENT (an empty VEVENT is invalid).
        if e.row.start_time.is_none() {
            continue;
        }
        fold_line("BEGIN:VEVENT", &mut out);
        let _ = vevent(
            e.status,
            e.row,
            e.locations,
            &VEventParty::default(),
            &mut out,
        );
        fold_line("END:VEVENT", &mut out);
    }
    calendar_close(&mut out);
    out
}

/// Filename slug for the per-event `Content-Disposition` — lowercase
/// alphanumerics and dashes, `event-{id}` when nothing survives.
pub fn ics_filename(name: Option<&str>, id: i64) -> String {
    let slug: String = name
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("event-{id}.ics")
    } else {
        // RFC 5987 not needed — the slug is ASCII-only.
        format!("{slug}-{id}.ics")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::JoinMode;
    use chrono::TimeZone;

    fn row() -> EventRow {
        EventRow {
            status_id: 1,
            start_time: Some(Utc.with_ymd_and_hms(2026, 12, 12, 19, 0, 0).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2026, 12, 12, 22, 0, 0).unwrap()),
            end_time_inferred: false,
            display_end_time: true,
            timezone: Some("Europe/Rome".into()),
            utc_offset_seconds: Some(3600),
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

    fn status() -> StatusRow {
        StatusRow {
            id: 1,
            account_id: 1,
            content: "Bring a jacket; it's cold".into(),
            content_html: Some("<p>Bring a jacket; it's cold</p>".into()),
            content_type: "text/html".into(),
            language: None,
            visibility: "public".into(),
            spoiler_text: None,
            sensitive: false,
            local: true,
            uri: Some("https://yonder.example/users/alice/statuses/1".into()),
            url: Some("https://yonder.example/@alice/1".into()),
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_uri: None,
            reblog_of_id: None,
            quote_of_id: None,
            quote_uri: None,
            quote_authorization_uri: None,
            original_id: None,
            original_created_at: None,
            object_type: Some("Event".into()),
            title: Some("Winter meetup; outdoors".into()),
            summary: None,
            cover_media_id: None,
            poll_id: None,
            replies_count: 0,
            reblogs_count: 0,
            favourites_count: 0,
            quotes_count: 0,
            edited_at: None,
            remote_likes_count: None,
            remote_shares_count: None,
            remote_quotes_count: None,
            remote_counts_fetched_at: None,
            last_selected_at: None,
            fetched_at: Utc::now(),
            tombstone: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn escapes_text_values() {
        assert_eq!(escape_text("a;b,c\\d\ne"), "a\\;b\\,c\\\\d\\ne");
        assert_eq!(escape_text("x\ry"), "xy");
    }

    #[test]
    fn folds_at_75_octets_on_char_boundaries() {
        let long = "é".repeat(60); // 120 bytes
        let mut out = String::new();
        fold_line(&format!("SUMMARY:{long}"), &mut out);
        let lines: Vec<&str> = out.trim_end().split("\r\n").collect();
        assert!(lines.len() >= 2);
        for l in &lines {
            assert!(l.len() <= 75, "line too long: {}", l.len());
        }
        assert!(lines[1..].iter().all(|l| l.starts_with(' ')));
        // Reassembled (minus fold spaces) equals the input.
        let joined: String = lines
            .iter()
            .map(|l| l.strip_prefix(' ').unwrap_or(l))
            .collect();
        assert_eq!(joined, format!("SUMMARY:{long}"));
    }

    #[test]
    fn vevent_uses_tzid_and_omits_dtend_when_hidden() {
        let s = status();
        let mut r = row();
        r.display_end_time = false;
        let mut out = String::new();
        vevent(&s, &r, &[], &VEventParty::default(), &mut out).unwrap();
        assert!(out.contains("DTSTART;TZID=Europe/Rome:20261212T200000"));
        assert!(!out.contains("DTEND"));
        assert!(out.contains("STATUS:CONFIRMED"));
        assert!(out.contains("SUMMARY:Winter meetup\\; outdoors"));
        assert!(out.contains("UID:https://yonder.example/users/alice/statuses/1"));
    }

    #[test]
    fn vevent_cancelled_and_gregorian() {
        let s = status();
        let mut r = row();
        r.status = EventStatus::Cancelled;
        r.display_end_time = true;
        let mut out = String::new();
        vevent(&s, &r, &[], &VEventParty::default(), &mut out).unwrap();
        assert!(out.contains("STATUS:CANCELLED"));
        assert!(out.contains("DTEND;TZID=Europe/Rome:20261212T230000"));
    }

    #[test]
    fn postponed_event_emits_no_vevent() {
        let s = status();
        let mut r = row();
        r.start_time = None;
        r.end_time = None;
        let mut out = String::new();
        vevent(&s, &r, &[], &VEventParty::default(), &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn filename_slug_sanitizes() {
        assert_eq!(
            ics_filename(Some("Winter Meetup!"), 7),
            "winter-meetup-7.ics"
        );
        assert_eq!(ics_filename(Some("!!!"), 7), "event-7.ics");
        assert_eq!(ics_filename(None, 7), "event-7.ics");
    }
}
