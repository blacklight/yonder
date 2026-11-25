//! Event preamble — the "When / Where / Online" block shared by the
//! Mastodon-API read path (status `content` prefix) and, later, the
//! ActivityPub `summary` (wire format section).
//!
//! Labels come from a small static table keyed by the event language
//! (`en`/`es`/`it`/`de`/`ar` — `SUPPORTED_LOCALES`), falling back to
//! English. Dates are language-neutral: `YYYY-MM-DD HH:MM` in the
//! event zone, plus the zone id and UTC offset.

use chrono::{DateTime, TimeZone, Utc};

use super::model::{EventLocationRow, EventRow, EventStatus, LocationKind};

/// Locales the label table covers — mirrors `SUPPORTED_LOCALES`.
const LABELS: &[(&str, Labels)] = &[
    ("en", Labels::EN),
    ("es", Labels::ES),
    ("it", Labels::IT),
    ("de", Labels::DE),
    ("ar", Labels::AR),
];

/// The translatable strings one preamble needs.
#[derive(Debug, Clone, Copy)]
pub struct Labels {
    pub when: &'static str,
    pub where_: &'static str,
    pub online: &'static str,
    pub cancelled: &'static str,
    pub postponed: &'static str,
    pub tentative: &'static str,
    pub rescheduled_from: &'static str,
    pub was: &'static str,
}

impl Labels {
    const EN: Labels = Labels {
        when: "When",
        where_: "Where",
        online: "Online",
        cancelled: "Cancelled",
        postponed: "Postponed",
        tentative: "Tentative",
        rescheduled_from: "Rescheduled from",
        was: "was",
    };
    const ES: Labels = Labels {
        when: "Cuándo",
        where_: "Dónde",
        online: "En línea",
        cancelled: "Cancelado",
        postponed: "Aplazado",
        tentative: "Provisional",
        rescheduled_from: "Reprogramado desde",
        was: "era",
    };
    const IT: Labels = Labels {
        when: "Quando",
        where_: "Dove",
        online: "Online",
        cancelled: "Annullato",
        postponed: "Posticipato",
        tentative: "Provvisorio",
        rescheduled_from: "Rinviato da",
        was: "era",
    };
    const DE: Labels = Labels {
        when: "Wann",
        where_: "Wo",
        online: "Online",
        cancelled: "Abgesagt",
        postponed: "Verschoben",
        tentative: "Vorläufig",
        rescheduled_from: "Verschoben von",
        was: "war",
    };
    const AR: Labels = Labels {
        when: "متى",
        where_: "أين",
        online: "عبر الإنترنت",
        cancelled: "ملغى",
        postponed: "مؤجل",
        tentative: "مؤقت",
        rescheduled_from: "أعيدت جدولته من",
        was: "كان",
    };
}

/// Labels for an event language (`"fr"` → English fallback).
pub fn labels_for(language: Option<&str>) -> Labels {
    let lang = language
        .unwrap_or("en")
        .split(['-', '_'])
        .next()
        .unwrap_or("en");
    LABELS
        .iter()
        .find(|(code, _)| *code == lang)
        .map(|(_, l)| *l)
        .unwrap_or(Labels::EN)
}

/// `dt` as a naive datetime in the event zone: the IANA `timezone`
/// when known, else the literal UTC offset, else UTC.
fn local_naive(row: &EventRow, dt: DateTime<Utc>) -> chrono::NaiveDateTime {
    if let Some(tz_name) = row.timezone.as_deref() {
        if let Ok(tz) = crate::events::model::TimezoneId::try_from(tz_name) {
            return tz.0.from_utc_datetime(&dt.naive_utc()).naive_local();
        }
    }
    if let Some(offset) = row.utc_offset_seconds {
        return (dt + chrono::Duration::seconds(offset as i64)).naive_utc();
    }
    dt.naive_utc()
}

/// `dt` rendered in the event zone as `YYYY-MM-DD HH:MM`.
fn format_in_zone(row: &EventRow, dt: DateTime<Utc>) -> String {
    local_naive(row, dt).format("%Y-%m-%d %H:%M").to_string()
}

/// Zone tag for the `When:` line — the IANA id, `UTC±HH:MM`, or `UTC`.
fn zone_tag(row: &EventRow) -> String {
    if let Some(tz_name) = row.timezone.as_deref() {
        if crate::events::model::TimezoneId::try_from(tz_name).is_ok() {
            return tz_name.to_string();
        }
    }
    if let Some(offset) = row.utc_offset_seconds {
        return utc_offset_label(offset);
    }
    "UTC".to_string()
}

/// `UTC+02:00` label for a literal UTC offset in seconds — shared by
/// the preamble's zone tag and `yonder.event.utc_offset`.
pub fn utc_offset_label(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
}

/// The map-search query for a place: structured parts joined, else
/// the freeform `address_text` — mirrors the frontend's
/// `locationText` (the venue name alone is never a query).
fn place_query(loc: &EventLocationRow) -> String {
    let structured: Vec<&str> = [
        loc.street_address.as_deref(),
        loc.locality.as_deref(),
        loc.region.as_deref(),
        loc.postal_code.as_deref(),
        loc.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !structured.is_empty() {
        return structured.join(", ");
    }
    loc.address_text.clone().unwrap_or_default()
}

/// Percent-encode a map-search query value.
fn encode_query(q: &str) -> String {
    const QUERY_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(q, QUERY_SET).to_string()
}

/// Map link for a place location, following `[events] map_provider`
/// — coordinates win, the address text is the fallback query. `None`
/// when the place has neither (mirrors the SPA's `placeMapUrl`).
pub fn place_map_url(
    loc: &EventLocationRow,
    provider: crate::config::EventMapProvider,
) -> Option<String> {
    use crate::config::EventMapProvider;
    let coords = match (loc.latitude, loc.longitude) {
        (Some(lat), Some(lon)) => Some((lat, lon)),
        _ => None,
    };
    match provider {
        EventMapProvider::Google => {
            let q = match coords {
                Some((lat, lon)) => format!("{lat},{lon}"),
                None => {
                    let text = place_query(loc);
                    if text.is_empty() {
                        return None;
                    }
                    text
                }
            };
            Some(format!(
                "https://www.google.com/maps/search/?api=1&query={}",
                encode_query(&q)
            ))
        }
        EventMapProvider::Osm => {
            if let Some((lat, lon)) = coords {
                return Some(format!(
                    "https://www.openstreetmap.org/?mlat={lat}&mlon={lon}#map=15/{lat}/{lon}"
                ));
            }
            let text = place_query(loc);
            if text.is_empty() {
                return None;
            }
            Some(format!(
                "https://www.openstreetmap.org/search?query={}",
                encode_query(&text)
            ))
        }
    }
}

/// One-line place rendering: `name, street locality region postal,
/// country` — falls back to `address_text`, then the bare name.
fn place_line(loc: &EventLocationRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = &loc.name {
        parts.push(name.clone());
    }
    let structured: Vec<&str> = [
        loc.street_address.as_deref(),
        loc.locality.as_deref(),
        loc.region.as_deref(),
        loc.postal_code.as_deref(),
        loc.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !structured.is_empty() {
        parts.push(structured.join(", "));
    } else if let Some(text) = &loc.address_text {
        // Whole-address fallback (e.g. Mobilizon's all-null
        // PostalAddress carrying the address in `location.name`).
        parts.push(text.clone());
    }
    parts.join(", ")
}

/// The read-time preamble HTML: an optional status line, then
/// `When:`/`Where:`/`Online:` rows joined by `<br>` inside one `<p>`.
/// Returns an empty string when the event carries no displayable
/// facts (no start, no locations).
pub fn preamble_html(
    row: &EventRow,
    locations: &[EventLocationRow],
    language: Option<&str>,
    map_provider: crate::config::EventMapProvider,
) -> String {
    let labels = labels_for(language);
    let mut lines: Vec<String> = Vec::new();

    // Status line first — a cancelled event announces itself before
    // any logistics.
    match row.status {
        EventStatus::Cancelled => lines.push(labels.cancelled.to_string()),
        EventStatus::Tentative => lines.push(labels.tentative.to_string()),
        EventStatus::Postponed => {
            let mut line = labels.postponed.to_string();
            if let Some(prev) = row.previous_start_time {
                line.push_str(&format!(
                    " ({} {})",
                    labels.was,
                    html_escape_text(&format_instant(prev, row))
                ));
            }
            lines.push(line);
        }
        EventStatus::Rescheduled => {
            if let Some(prev) = row.previous_start_time {
                lines.push(format!(
                    "{} {}",
                    labels.rescheduled_from,
                    html_escape_text(&format_instant(prev, row))
                ));
            }
        }
        _ => {}
    }

    if let Some(start) = row.start_time {
        let mut when = format!("{}: {}", labels.when, format_in_zone(row, start));
        if row.display_end_time {
            if let Some(end) = row.end_time {
                when.push_str(&format!(" – {}", end_in_zone(row, start, end)));
            }
        }
        when.push_str(&format!(" ({})", zone_tag(row)));
        lines.push(when);
    }

    // `place_line` output is remote text — escape before it enters
    // the HTML `content` payload. Places with coordinates or an
    // address link to the configured map provider, like the SPA.
    let places: Vec<String> = locations
        .iter()
        .filter(|l| l.kind == LocationKind::Place)
        .filter_map(|l| {
            let line = place_line(l);
            if line.trim().is_empty() {
                return None;
            }
            let escaped = html_escape_text(&line);
            Some(match place_map_url(l, map_provider) {
                Some(url) => format!("<a href=\"{}\">{}</a>", html_escape_text(&url), escaped),
                None => escaped,
            })
        })
        .collect();
    if !places.is_empty() {
        lines.push(format!("{}: {}", labels.where_, places.join("; ")));
    }
    let online: Vec<String> = locations
        .iter()
        .filter(|l| l.kind == LocationKind::Virtual)
        .filter_map(|l| {
            let name = l
                .name
                .clone()
                .unwrap_or_else(|| l.url.clone().unwrap_or_default());
            match &l.url {
                Some(url) if crate::events::remote::url_scheme_ok(url, false) => Some(format!(
                    "<a href=\"{}\">{}</a>",
                    html_escape_text(url),
                    html_escape_text(&name)
                )),
                _ => (!name.is_empty()).then_some(html_escape_text(&name)),
            }
        })
        .collect();
    if !online.is_empty() {
        lines.push(format!("{}: {}", labels.online, online.join(", ")));
    }

    if lines.is_empty() {
        return String::new();
    }
    format!("<p>{}</p>", lines.join("<br>"))
}

/// `dt` rendered RFC 3339 in the event zone — the IANA `timezone`
/// when known, else the literal UTC offset, else UTC. Gancio style:
/// `2026-12-12T20:00:00+01:00`.
pub fn rfc3339_in_zone(row: &EventRow, dt: DateTime<Utc>) -> String {
    if let Some(tz_name) = row.timezone.as_deref() {
        if let Ok(tz) = crate::events::model::TimezoneId::try_from(tz_name) {
            return tz
                .0
                .from_utc_datetime(&dt.naive_utc())
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string();
        }
    }
    if let Some(offset) = row.utc_offset_seconds {
        if let Some(fixed) = chrono::FixedOffset::east_opt(offset) {
            return fixed
                .from_utc_datetime(&dt.naive_utc())
                .format("%Y-%m-%dT%H:%M:%S%:z")
                .to_string();
        }
    }
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Void/standalone elements that never need a closing tag when an
/// HTML excerpt is truncated.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Truncate a sanitized HTML fragment to at most `max` text
/// characters, preserving markup: tags pass through, `&entities;`
/// count as one character, `<img>` tags are dropped (preview noise —
/// their caption text stays), and when the text overruns the output
/// ends with an ellipsis plus every still-open tag closed. Returns
/// the fragment unchanged when it fits.
fn truncate_html_excerpt(html: &str, max: usize) -> String {
    // First pass: count text characters (entities as one) — no
    // truncation needed when the fragment already fits.
    let mut total = 0usize;
    let mut rest = html;
    while let Some(pos) = rest.find(['<', '&']) {
        total += rest[..pos].chars().count();
        rest = &rest[pos..];
        if rest.starts_with('<') {
            match rest.find('>') {
                Some(end) => rest = &rest[end + 1..],
                None => {
                    total += rest.chars().count();
                    rest = "";
                }
            }
        } else {
            // `&entity;` — one visible character when terminated,
            // literal text otherwise.
            match rest.find(';') {
                Some(end) if end <= 12 && !rest[1..end].contains(|c: char| c.is_whitespace()) => {
                    total += 1;
                    rest = &rest[end + 1..];
                }
                _ => {
                    total += 1;
                    rest = &rest[1..];
                }
            }
        }
    }
    total += rest.chars().count();
    if total <= max {
        return drop_imgs(html);
    }

    // Second pass: copy through, stopping mid-text at the budget.
    let mut out = String::with_capacity(html.len());
    let mut open: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut rest = html;
    let mut done = false;
    while !rest.is_empty() && !done {
        if let Some(stripped) = rest.strip_prefix('<') {
            let end = match stripped.find('>') {
                Some(end) => end,
                None => break, // unterminated trailing `<` — drop it
            };
            let tag_src = &stripped[..end];
            let closing = tag_src.starts_with('/');
            let name: String = tag_src
                .trim_start_matches('/')
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            let self_closing = tag_src.ends_with('/') || VOID_ELEMENTS.contains(&name.as_str());
            if name != "img" {
                out.push('<');
                out.push_str(tag_src);
                out.push('>');
            }
            if closing {
                if let Some(pos) = open.iter().rposition(|n| *n == name) {
                    open.remove(pos);
                }
            } else if !self_closing && !name.is_empty() {
                open.push(name);
            }
            rest = &stripped[end + 1..];
        } else {
            // Text run up to the next tag; entities count as one char.
            let run_end = rest.find('<').unwrap_or(rest.len());
            let mut chars = rest[..run_end].chars().peekable();
            while let Some(c) = chars.next() {
                if count >= max {
                    done = true;
                    break;
                }
                if c == '&' {
                    // Copy a terminated entity verbatim as one char.
                    let mut entity = String::from('&');
                    let mut terminated = false;
                    while let Some(&next) = chars.peek() {
                        if next == ';' || next.is_whitespace() || next == '&' || entity.len() > 11 {
                            terminated = next == ';';
                            if terminated {
                                entity.push(';');
                                chars.next();
                            }
                            break;
                        }
                        entity.push(next);
                        chars.next();
                    }
                    let _ = terminated;
                    out.push_str(&entity);
                } else {
                    out.push(c);
                }
                count += 1;
            }
            rest = &rest[run_end..];
        }
    }
    out.push('…');
    while let Some(name) = open.pop() {
        out.push_str(&format!("</{name}>"));
    }
    out
}

/// Drop `<img …>` tags from a fragment — they are preview noise in a
/// generated summary (the caption text the renderer emits after them
/// is unaffected).
fn drop_imgs(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("<img") {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        match rest.find('>') {
            Some(end) => rest = &rest[end + 1..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out
}

/// The ActivityPub `summary` form (plan.md §5.2): the preamble lines
/// plus a description excerpt capped at `excerpt_chars` visible
/// characters (`0` = preamble only). The excerpt is the status's
/// already-rendered, already-sanitized `content_html` — never raw
/// markdown, so links and emphasis survive into Mastodon-style
/// clients that render the summary in place of the content — with
/// images dropped and the tail closed cleanly when truncated.
pub fn ap_summary_html(
    row: &EventRow,
    locations: &[EventLocationRow],
    language: Option<&str>,
    description_html: Option<&str>,
    excerpt_chars: u32,
    map_provider: crate::config::EventMapProvider,
) -> String {
    let mut html = preamble_html(row, locations, language, map_provider);
    if excerpt_chars > 0 {
        if let Some(desc) = description_html.map(str::trim).filter(|t| !t.is_empty()) {
            let excerpt = truncate_html_excerpt(desc, excerpt_chars as usize);
            if html.is_empty() {
                html = excerpt;
            } else {
                html.push_str(&excerpt);
            }
        }
    }
    html
}

/// `end` rendered in the event zone: `HH:MM` when it falls on the
/// same local date as `start`, else `YYYY-MM-DD HH:MM` (cross-midnight
/// and multi-day events keep the end's own date).
fn end_in_zone(row: &EventRow, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let end_local = local_naive(row, end);
    if end_local.date() == local_naive(row, start).date() {
        end_local.format("%H:%M").to_string()
    } else {
        end_local.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Compact instant for "was …"/"rescheduled from …" clauses.
fn format_instant(dt: DateTime<Utc>, row: &EventRow) -> String {
    let mut s = dt.format("%Y-%m-%d %H:%M").to_string();
    if let Some(tz) = row.timezone.as_deref() {
        s.push_str(&format!(" {tz}"));
    }
    s
}

fn html_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EventMapProvider;
    use crate::events::model::{EventRow, EventStatus};

    fn row() -> EventRow {
        // Build via SQL-free literal — fields are pub.
        EventRow {
            status_id: 1,
            start_time: Some("2026-12-12T19:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            end_time: Some("2026-12-12T22:00:00Z".parse::<DateTime<Utc>>().unwrap()),
            end_time_inferred: false,
            display_end_time: true,
            timezone: Some("Europe/Rome".into()),
            utc_offset_seconds: Some(3600),
            status: EventStatus::Scheduled,
            previous_start_time: None,
            join_mode: crate::events::model::JoinMode::Free,
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
            attendees_visibility: crate::events::model::AttendeesVisibility::Organizers,
            attendees_uri: None,
            remote_join_protocol: None,
            raw_status: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn loc(kind: LocationKind, name: &str, url: Option<&str>) -> EventLocationRow {
        EventLocationRow {
            id: 1,
            status_id: 1,
            position: 0,
            kind,
            uri: None,
            name: Some(name.into()),
            url: url.map(str::to_string),
            address_text: None,
            street_address: None,
            locality: None,
            region: None,
            postal_code: None,
            country: None,
            latitude: None,
            longitude: None,
        }
    }

    #[test]
    fn scheduled_event_renders_when_where_online() {
        let mut place = loc(LocationKind::Place, "Circolo Arci", None);
        place.street_address = Some("Via Roma 1".into());
        place.locality = Some("Roma".into());
        let virtual_loc = loc(
            LocationKind::Virtual,
            "Jitsi",
            Some("https://jitsi.example/room"),
        );
        let html = preamble_html(
            &row(),
            &[place, virtual_loc],
            Some("en"),
            EventMapProvider::Osm,
        );
        assert!(html.contains("When: 2026-12-12 20:00 – 23:00"), "{html}");
        assert!(html.contains("(Europe/Rome)"), "{html}");
        assert!(html.contains("Circolo Arci, Via Roma 1, Roma"), "{html}");
        assert!(
            html.contains("Online: <a href=\"https://jitsi.example/room\">Jitsi</a>"),
            "{html}"
        );
    }

    #[test]
    fn place_with_coordinates_links_osm_pin() {
        let mut place = loc(LocationKind::Place, "MIT", None);
        place.latitude = Some(42.3582529);
        place.longitude = Some(-71.0966272);
        let html = preamble_html(&row(), &[place], Some("en"), EventMapProvider::Osm);
        assert!(
            html.contains(
                "Where: <a href=\"https://www.openstreetmap.org/?mlat=42.3582529&amp;mlon=-71.0966272#map=15/42.3582529/-71.0966272\">MIT</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn place_with_address_text_links_osm_search() {
        let mut place = loc(LocationKind::Place, "Circolo Arci", None);
        place.address_text = Some("Via Roma 1, Roma".into());
        let html = preamble_html(&row(), &[place], Some("en"), EventMapProvider::Osm);
        assert!(
            html.contains(
                "<a href=\"https://www.openstreetmap.org/search?query=Via%20Roma%201%2C%20Roma\">Circolo Arci, Via Roma 1, Roma</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn google_provider_links_google_maps() {
        let mut place = loc(LocationKind::Place, "MIT", None);
        place.latitude = Some(42.3582529);
        place.longitude = Some(-71.0966272);
        let html = preamble_html(&row(), &[place], Some("en"), EventMapProvider::Google);
        assert!(
            html.contains(
                "<a href=\"https://www.google.com/maps/search/?api=1&amp;query=42.3582529%2C-71.0966272\">MIT</a>"
            ),
            "{html}"
        );
    }

    #[test]
    fn bare_name_place_has_no_map_link() {
        // No coordinates and no address — nothing to query, so the
        // place line stays plain text (mirrors the SPA).
        let place = loc(LocationKind::Place, "Somewhere", None);
        let html = preamble_html(&row(), &[place], Some("en"), EventMapProvider::Osm);
        assert!(html.contains("Where: Somewhere"), "{html}");
        assert!(!html.contains("openstreetmap"), "{html}");
    }

    #[test]
    fn summary_excerpt_keeps_rendered_links() {
        let desc = "<p>Compliant with <a href=\"https://example.org/fep\" rel=\"nofollow noopener\">FEP-8a8e</a></p>\n";
        let html = ap_summary_html(
            &row(),
            &[],
            Some("en"),
            Some(desc),
            300,
            EventMapProvider::Osm,
        );
        assert!(
            html.contains(
                "<a href=\"https://example.org/fep\" rel=\"nofollow noopener\">FEP-8a8e</a>"
            ),
            "{html}"
        );
        assert!(!html.contains(']'), "no raw markdown leaks: {html}");
    }

    #[test]
    fn summary_excerpt_truncates_cleanly() {
        let desc = "<p>alpha beta gamma delta <strong>epsilon zeta</strong> eta theta</p>";
        // 30 visible chars: the cut lands inside `<strong>` (7 chars
        // into "epsilon zeta"), so both tags must close after the
        // ellipsis — no dangling markup.
        let html = ap_summary_html(
            &row(),
            &[],
            Some("en"),
            Some(desc),
            30,
            EventMapProvider::Osm,
        );
        assert!(
            html.contains("<p>alpha beta gamma delta <strong>epsilon…</strong></p>"),
            "{html}"
        );
        assert!(!html.contains("theta"), "{html}");
    }

    #[test]
    fn summary_excerpt_drops_images() {
        let desc = "<p>look <img src=\"https://x/p.png\" alt=\"pic\"> caption here</p>";
        let html = ap_summary_html(
            &row(),
            &[],
            Some("en"),
            Some(desc),
            300,
            EventMapProvider::Osm,
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("caption here"), "{html}");
    }

    #[test]
    fn cancelled_leads_with_status_line() {
        let mut r = row();
        r.status = EventStatus::Cancelled;
        let html = preamble_html(&r, &[], Some("en"), EventMapProvider::Osm);
        assert!(html.starts_with("<p>Cancelled<br>"), "{html}");
    }

    #[test]
    fn inferred_end_is_never_displayed() {
        let mut r = row();
        r.display_end_time = false;
        r.end_time_inferred = true;
        let html = preamble_html(&r, &[], Some("en"), EventMapProvider::Osm);
        assert!(html.contains("20:00"), "{html}");
        assert!(!html.contains("23:00"), "{html}");
    }

    #[test]
    fn postponed_without_start_says_was() {
        let mut r = row();
        r.status = EventStatus::Postponed;
        r.start_time = None;
        r.end_time = None;
        r.previous_start_time = Some("2026-12-12T19:00:00Z".parse().unwrap());
        let html = preamble_html(&r, &[], Some("en"), EventMapProvider::Osm);
        assert!(html.contains("Postponed (was 2026-12-12 19:00"), "{html}");
        assert!(!html.contains("When:"), "{html}");
    }

    #[test]
    fn remote_location_text_is_html_escaped() {
        // Remote-supplied names must not inject markup into `content`.
        let mut place = loc(LocationKind::Place, "<img src=x onerror=alert(1)>", None);
        place.address_text = Some("Hall & \"Annex\"".into());
        let virtual_loc = loc(LocationKind::Virtual, "<b>Meet</b>", None);
        let html = preamble_html(
            &row(),
            &[place, virtual_loc],
            Some("en"),
            EventMapProvider::Osm,
        );
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "{html}"
        );
        assert!(html.contains("Hall &amp; &quot;Annex&quot;"), "{html}");
        assert!(html.contains("&lt;b&gt;Meet&lt;/b&gt;"), "{html}");
    }

    #[test]
    fn multi_day_event_shows_end_date() {
        let mut r = row();
        r.start_time = Some("2026-12-12T19:00:00Z".parse().unwrap());
        r.end_time = Some("2026-12-14T22:00:00Z".parse().unwrap());
        let html = preamble_html(&r, &[], Some("en"), EventMapProvider::Osm);
        // Europe/Rome is UTC+1 in December: the end lands on a
        // different local date than the start, so it keeps its date.
        assert!(html.contains("2026-12-14 23:00"), "{html}");
        // Same-day ends keep the compact HH:MM form.
        let mut r2 = row();
        r2.start_time = Some("2026-12-12T19:00:00Z".parse().unwrap());
        r2.end_time = Some("2026-12-12T22:00:00Z".parse().unwrap());
        let html2 = preamble_html(&r2, &[], Some("en"), EventMapProvider::Osm);
        assert!(html2.contains(" – 23:00"), "{html2}");
    }

    #[test]
    fn unknown_language_falls_back_to_english() {
        assert_eq!(labels_for(Some("fr")).when, "When");
        assert_eq!(labels_for(Some("it")).when, "Quando");
        assert_eq!(labels_for(Some("de-DE")).where_, "Wo");
        assert_eq!(labels_for(None).when, "When");
    }
}
