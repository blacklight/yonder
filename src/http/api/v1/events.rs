//! `/api/v1/yonder/events*` — Yonder event extensions: the per-event
//! ICS export that backs the SPA's "Download calendar" action.
//! Anonymous access mirrors an unauthenticated status fetch:
//! `public`/`unlisted` events serve without a token (calendar apps
//! cannot send `Authorization` headers); `followers_only`, `direct`
//! and `local` events require a token and pass the usual
//! [`posts::visible_ids`] check.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
    Json,
};

use crate::{
    events::{api, entity, ics},
    http::{middleware::auth::MaybeAuth, AppState},
    posts::{self, PostContext},
    Error, Result,
};

/// `GET /api/v1/yonder/events/{id}/ics` — RFC 5545 `VCALENDAR` for one
/// event. `id` is the event's status id.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/events/{id}/ics",
    tag = "statuses",
    params(("id" = i64, Path, description = "Status id of the event")),
    responses(
        (status = 200, description = "iCalendar feed for the event", content_type = "text/calendar"),
        (status = 401, description = "Missing token for a non-public event", body = crate::error::ErrorBody),
        (status = 404, description = "Not an event, or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn event_ics(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Response> {
    let status = posts::load(&state.db, id)
        .await?
        .filter(posts::is_event)
        .ok_or_else(|| Error::NotFound("event".into()))?;
    // Public-shaped events serve anonymously; anything narrower
    // needs a token that can actually see the status.
    if !matches!(status.visibility.as_str(), "public" | "unlisted") {
        let viewer = auth.0.ok_or(Error::Unauthorized)?.account_id;
        let visible =
            posts::visible_ids(&state.db, std::slice::from_ref(&status), Some(viewer)).await?;
        if !visible.contains(&status.id) {
            return Err(Error::NotFound("event".into()));
        }
    }
    let mut bundles = entity::load_many(&state.db, &[status.id], None).await?;
    let bundle = bundles
        .remove(&status.id)
        .ok_or_else(|| Error::NotFound("event".into()))?;
    let body = ics::event_ics(&status, &bundle.row, &bundle.locations)?
        .ok_or_else(|| Error::NotFound("event".into()))?;
    let filename = ics::ics_filename(status.title.as_deref(), status.id);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/calendar; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// RSVP + organizer management (Section 11)
// ---------------------------------------------------------------------------

fn ctx(state: &AppState) -> PostContext {
    PostContext::new(state)
}

/// `POST /api/v1/yonder/events/{id}/rsvp` — `{state, message?}`.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/events/{id}/rsvp",
    tag = "events",
    params(("id" = i64, Path, description = "Status id of the event")),
    request_body = api::RsvpBody,
    responses(
        (status = 200, description = "Participation state after the transition"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 404, description = "Not an event or not visible", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn event_rsvp(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<api::RsvpBody>,
) -> Result<Json<serde_json::Value>> {
    let result = api::rsvp(&ctx(&state), auth.account_id, id, &body).await?;
    Ok(Json(serde_json::json!({"participation": result})))
}

/// `POST /api/v1/yonder/events/{id}/invitees` — organizer-only.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/events/{id}/invitees",
    tag = "events",
    params(("id" = i64, Path, description = "Status id of the event")),
    request_body = api::InviteesBody,
    responses(
        (status = 200, description = "Invitees recorded"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 403, description = "Not an organizer", body = crate::error::ErrorBody),
        (status = 404, description = "Not an event or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn event_invitees(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<api::InviteesBody>,
) -> Result<Json<serde_json::Value>> {
    api::invitees(&ctx(&state), auth.account_id, id, &body).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /api/v1/yonder/events/{id}/dm-invites` — Catodon-style DM
/// invitations; works for events of any visibility.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/events/{id}/dm-invites",
    tag = "events",
    params(("id" = i64, Path, description = "Status id of the event")),
    request_body = api::InviteesBody,
    responses(
        (status = 200, description = "The invite DM status"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 403, description = "Not an organizer", body = crate::error::ErrorBody),
        (status = 404, description = "Not an event or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn event_dm_invites(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<api::InviteesBody>,
) -> Result<Json<serde_json::Value>> {
    let entity = api::dm_invites(&ctx(&state), &auth, auth.account_id, id, &body).await?;
    Ok(Json(entity))
}

/// `DELETE /api/v1/yonder/events/{id}/participants/{account_id}`.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/events/{id}/participants/{account_id}",
    tag = "events",
    params(
        ("id" = i64, Path, description = "Status id of the event"),
        ("account_id" = i64, Path, description = "Account id to remove"),
    ),
    responses(
        (status = 200, description = "Participant removed"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 403, description = "Not an organizer", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn event_remove_participant(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path((id, account_id)): Path<(i64, i64)>,
) -> Result<Json<serde_json::Value>> {
    api::remove_participant(&ctx(&state), auth.account_id, id, account_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /api/v1/yonder/events/{id}/participation_requests/{account_id}/authorize|reject`.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/events/{id}/participation_requests/{account_id}/{decision}",
    tag = "events",
    params(
        ("id" = i64, Path, description = "Status id of the event"),
        ("account_id" = i64, Path, description = "Account id deciding on"),
        ("decision" = String, Path, description = "authorize | reject"),
    ),
    responses(
        (status = 200, description = "Request decided"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 403, description = "Not an organizer", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
        (status = 422, description = "Unknown decision", body = crate::error::ErrorBody),
    )
)]
pub async fn event_decide_request(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path((id, account_id, decision)): Path<(i64, i64, String)>,
) -> Result<Json<serde_json::Value>> {
    let approve = match decision.as_str() {
        "authorize" => true,
        "reject" => false,
        _ => {
            return Err(Error::Validation(
                "decision must be authorize or reject".to_string(),
            ))
        }
    };
    api::decide_request(&ctx(&state), auth.account_id, id, account_id, approve).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `GET /api/v1/yonder/events/{id}/participations` — organizer-only
/// listing (pending requests + states for the management UI).
#[utoipa::path(
    get,
    path = "/api/v1/yonder/events/{id}/participations",
    tag = "events",
    params(("id" = i64, Path, description = "Status id of the event")),
    responses(
        (status = 200, description = "Participation rows"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 403, description = "Not an organizer", body = crate::error::ErrorBody),
        (status = 404, description = "Not an event or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn event_participations(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>> {
    let rows = api::list_participations(&ctx(&state), auth.account_id, id).await?;
    Ok(Json(serde_json::json!({"participations": rows})))
}

/// `GET /api/v1/yonder/geocoding/search?q=` — address autocomplete
/// for the composer, normalized across `[events.geocoding]`
/// providers; `[]` when `provider = "none"`.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/geocoding/search",
    tag = "events",
    params(("q" = String, Query, description = "Free-text place query")),
    responses(
        (status = 200, description = "Place suggestions in §9.1 location shape"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
        (status = 502, description = "Geocoder upstream failure", body = crate::error::ErrorBody),
    )
)]
pub async fn geocoding_search(
    State(state): State<Arc<AppState>>,
    _auth: crate::http::middleware::auth::AuthContext,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>> {
    let q = params.get("q").map(String::as_str).unwrap_or("");
    let config = state.config();
    let hits = crate::events::geocoding::search(&config.events.geocoding, q).await?;
    Ok(Json(serde_json::json!(hits)))
}

/// `POST /api/v1/yonder/calendar/token` — mint (or rotate) the
/// personalized calendar subscription token. The URL is returned
/// once; only its hash is stored.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/calendar/token",
    tag = "events",
    responses(
        (status = 200, description = "Fresh calendar subscription URL"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
    )
)]
pub async fn calendar_token_create(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
) -> Result<Json<serde_json::Value>> {
    let token = crate::events::calendar::mint(&state.db, auth.account_id).await?;
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(auth.account_id)
        .fetch_one(&state.db)
        .await?;
    let base = ctx(&state).base_url();
    Ok(Json(serde_json::json!({
        "calendar_url": format!("{}/users/{}/calendar/{}.ics", base.trim_end_matches('/'), username, token),
    })))
}

/// `DELETE /api/v1/yonder/calendar/token` — revoke the subscription.
#[utoipa::path(
    delete,
    path = "/api/v1/yonder/calendar/token",
    tag = "events",
    responses(
        (status = 200, description = "Token revoked"),
        (status = 401, description = "Missing token", body = crate::error::ErrorBody),
    )
)]
pub async fn calendar_token_revoke(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
) -> Result<Json<serde_json::Value>> {
    crate::events::calendar::revoke(&state.db, auth.account_id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ---------------------------------------------------------------------------
// `GET /api/v1/yonder/events` — the events list (Section 15)
// ---------------------------------------------------------------------------

/// `near=lat,lon` → `(lat, lon)`, range-checked. `None` when absent.
fn parse_near(raw: Option<&String>) -> Result<Option<(f64, f64)>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (lat, lon) = raw
        .split_once(',')
        .ok_or_else(|| Error::Validation("near must be lat,lon".to_string()))?;
    let lat: f64 = lat
        .trim()
        .parse()
        .map_err(|_| Error::Validation("near latitude must be numeric".to_string()))?;
    let lon: f64 = lon
        .trim()
        .parse()
        .map_err(|_| Error::Validation("near longitude must be numeric".to_string()))?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(Error::Validation("near out of range".to_string()));
    }
    Ok(Some((lat, lon)))
}

/// Longitude half-width (degrees) of the `near` bounding box. Longitude
/// degrees shrink poleward, so the widest band is at the poleward edge
/// of the latitude band; using the query latitude would under-cover
/// events north (or south) of it and drop in-radius matches.
fn lon_half_width_deg(lat: f64, radius_km: f64) -> f64 {
    let poleward = (lat.abs() + radius_km / 111.0).min(89.999);
    radius_km / (111.0 * poleward.to_radians().cos())
}

/// `radius_km` for `near` — default 50, must be a finite number,
/// clamped to 0.1–500 km.
fn parse_radius_km(raw: Option<&String>) -> Result<f64> {
    let Some(raw) = raw else {
        return Ok(50.0);
    };
    let radius: f64 = raw
        .parse()
        .map_err(|_| Error::Validation("radius_km must be numeric".to_string()))?;
    if !radius.is_finite() {
        return Err(Error::Validation("radius_km must be finite".to_string()));
    }
    Ok(radius.clamp(0.1, 500.0))
}

/// `GET /api/v1/yonder/events` — chronological event listing.
///
/// Scopes: `mine` (viewer organizes), `participating` (viewer has any
/// participation row), `account:{id}` (that account's events,
/// visibility-scoped), `home` (followed accounts), `local`
/// (local public+local visibility), `federated` (public only).
/// `state` filters the viewer's participation state; `from`/`to`
/// bound the window (default now → +90d; an explicit `from` without
/// `to` is unbounded); `category` filters; postponed
/// events (`start_time IS NULL`) always appear, sorted last.
/// `near=lat,lon` + `radius_km` keep only events with a physical
/// location within the radius (spherical law of cosines).
/// Pagination is keyset on `(start_time, status_id)` via `cursor`.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/events",
    tag = "events",
    params(
        ("scope" = Option<String>, Query, description = "mine|participating|home|local|federated|account:{id}"),
        ("state" = Option<String>, Query, description = "Viewer participation state filter"),
        ("from" = Option<String>, Query, description = "RFC 3339 lower bound"),
        ("to" = Option<String>, Query, description = "RFC 3339 upper bound"),
        ("category" = Option<String>, Query, description = "FEP category filter"),
        ("tag" = Option<String>, Query, description = "Hashtag filter (case-insensitive, leading # optional)"),
        ("online" = Option<bool>, Query, description = "true → only virtual-location events"),
        ("near" = Option<String>, Query, description = "lat,lon — keep events with a physical location within radius_km"),
        ("radius_km" = Option<f64>, Query, description = "Search radius for `near` (default 50, max 500)"),
        ("include_cancelled" = Option<bool>, Query),
        ("cursor" = Option<String>, Query, description = "Keyset cursor from a previous page"),
        ("limit" = Option<i64>, Query),
    ),
    responses(
        (status = 200, description = "Event statuses, oldest start first"),
    )
)]
pub async fn events_list(
    State(state): State<Arc<AppState>>,
    auth: crate::http::middleware::auth::AuthContext,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    uri: axum::http::Uri,
) -> Result<Response> {
    let viewer = auth.account_id;
    let scope = params.get("scope").map(String::as_str).unwrap_or("mine");
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, 40);

    // Scope → status-id candidate set.
    let ids: Vec<i64> = match scope {
        "mine" => {
            sqlx::query_scalar(
                "SELECT p.status_id FROM event_participations p \
             WHERE p.account_id = $1 AND p.role = 'organizer'",
            )
            .bind(viewer)
            .fetch_all(&state.db)
            .await?
        }
        "participating" => {
            let mut q = String::from(
                "SELECT p.status_id FROM event_participations p WHERE p.account_id = $1",
            );
            if let Some(st) = params.get("state") {
                q.push_str(" AND p.state = $2");
                let rows: Vec<i64> = if st == "going" {
                    sqlx::query_scalar(
                        "SELECT p.status_id FROM event_participations p \
                         WHERE p.account_id = $1 AND p.state IN ('accepted','tentative')",
                    )
                    .bind(viewer)
                    .fetch_all(&state.db)
                    .await?
                } else {
                    sqlx::query_scalar(&q)
                        .bind(viewer)
                        .bind(st)
                        .fetch_all(&state.db)
                        .await?
                };
                rows
            } else {
                sqlx::query_scalar(&q)
                    .bind(viewer)
                    .fetch_all(&state.db)
                    .await?
            }
        }
        s if s.starts_with("account:") => {
            let account_id: i64 = s[8..]
                .parse()
                .map_err(|_| Error::Validation("account:{id} scope needs a numeric id".into()))?;
            sqlx::query_scalar(
                "SELECT s.id FROM statuses s \
                 WHERE s.account_id = $1 AND s.object_type = 'Event'",
            )
            .bind(account_id)
            .fetch_all(&state.db)
            .await?
        }
        "home" => {
            // Events authored by accounts the viewer follows.
            sqlx::query_scalar(
                "SELECT s.id FROM statuses s \
                 JOIN follows f ON f.target_account_id = s.account_id \
                 WHERE f.account_id = $1 AND s.object_type = 'Event' \
                   AND NOT s.tombstone",
            )
            .bind(viewer)
            .fetch_all(&state.db)
            .await?
        }
        // `federated`: public events everywhere. `local`: public +
        // local-visibility events from this instance only. Neither
        // includes `unlisted` — plan §8 forbids calendar/search output
        // without Public in `to`.
        "local" | "federated" => {
            let vis = if scope == "federated" {
                "'public'"
            } else {
                "'public','local'"
            };
            let local_only = if scope == "local" { "AND s.local " } else { "" };
            sqlx::query_scalar(&format!(
                "SELECT s.id FROM statuses s \
                 JOIN events e ON e.status_id = s.id \
                 WHERE s.object_type = 'Event' AND NOT s.tombstone \
                   {local_only}AND s.visibility IN ({vis})"
            ))
            .fetch_all(&state.db)
            .await?
        }
        _ => {
            return Err(Error::Validation(
                "scope must be mine, participating, home, local, federated or account:{id}"
                    .to_string(),
            ))
        }
    };

    // §16 "near me": keep only candidates with a physical location
    // within `radius_km` of `near=lat,lon`. A bounding box on
    // lat/lon (1 deg lat ≈ 111 km) prefilters on
    // `event_locations_geo_idx` before the distance check.
    let ids = match parse_near(params.get("near"))? {
        Some((lat, lon)) => {
            let radius_km = parse_radius_km(params.get("radius_km"))?;
            let lat_delta = radius_km / 111.0;
            let lon_delta = lon_half_width_deg(lat, radius_km);
            // The longitude box can wrap past ±180°; skip it there
            // rather than miss antimeridian events (the distance
            // check still filters).
            let lon_box = lon - lon_delta >= -180.0 && lon + lon_delta <= 180.0;
            sqlx::query_scalar(
                "SELECT l.status_id FROM event_locations l \
                 WHERE l.status_id = ANY($1) \
                   AND l.latitude IS NOT NULL AND l.longitude IS NOT NULL \
                   AND l.latitude BETWEEN $2 - $4 AND $2 + $4 \
                   AND ($7 OR l.longitude BETWEEN $3 - $5 AND $3 + $5) \
                   AND (6371.0 * acos(greatest(-1.0, least(1.0, \
                     cos(radians($2)) * cos(radians(l.latitude)) \
                       * cos(radians(l.longitude) - radians($3)) \
                     + sin(radians($2)) * sin(radians(l.latitude))))) <= $6",
            )
            .bind(&ids)
            .bind(lat)
            .bind(lon)
            .bind(lat_delta)
            .bind(lon_delta)
            .bind(radius_km)
            .bind(!lon_box)
            .fetch_all(&state.db)
            .await?
        }
        None => ids,
    };

    // §16 discovery filters: hashtag and online-only.
    let ids = match params
        .get("tag")
        .map(|s| s.trim().trim_start_matches('#'))
        .filter(|t| !t.is_empty())
    {
        Some(tag) => {
            sqlx::query_scalar(
                "SELECT st.status_id FROM status_tags st JOIN tags t ON t.id = st.tag_id \
                 WHERE st.status_id = ANY($1) AND lower(t.name) = lower($2)",
            )
            .bind(&ids)
            .bind(tag)
            .fetch_all(&state.db)
            .await?
        }
        None => ids,
    };
    let ids = if params
        .get("online")
        .is_some_and(|v| v == "true" || v == "1")
    {
        sqlx::query_scalar(
            "SELECT e.status_id FROM events e \
             WHERE e.status_id = ANY($1) \
               AND NOT EXISTS (SELECT 1 FROM event_locations l \
                 WHERE l.status_id = e.status_id AND l.kind <> 'virtual') \
               AND EXISTS (SELECT 1 FROM event_locations l \
                 WHERE l.status_id = e.status_id AND l.kind = 'virtual')",
        )
        .bind(&ids)
        .fetch_all(&state.db)
        .await?
    } else {
        ids
    };
    if ids.is_empty() {
        return Ok((
            [(header::CONTENT_TYPE, "application/json")],
            Json(serde_json::json!([])),
        )
            .into_response());
    }

    // Time window: default now → +90d; postponed rows always pass.
    // An explicit `from` without `to` is unbounded — otherwise
    // "include past" style queries (from=2000) would collapse the
    // window to `from + 90d` and hide everything.
    let from_explicit = params.contains_key("from");
    let from = params
        .get("from")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    let to = params
        .get("to")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
        .or(if from_explicit {
            None
        } else {
            Some(from + chrono::Duration::days(90))
        });
    let include_cancelled = params
        .get("include_cancelled")
        .is_some_and(|v| v == "true" || v == "1");
    let category = params.get("category").cloned();

    // Cursor: "{rfc3339|-inf|inf}|{status_id}" of the last row —
    // keyset on (COALESCE(start_time,'infinity'), status_id).
    // Absent cursor = page one (`-infinity` start); `inf` = the
    // postponed tail.
    let (cur_start, cur_id): (String, i64) = params
        .get("cursor")
        .and_then(|c| c.split_once('|'))
        .map(|(a, b)| {
            let key = match a {
                "inf" => "infinity".to_string(),
                "-inf" => "-infinity".to_string(),
                other => chrono::DateTime::parse_from_rfc3339(other)
                    .map(|d| {
                        d.with_timezone(&chrono::Utc)
                            .format("%Y-%m-%dT%H:%M:%S%.fZ")
                            .to_string()
                    })
                    .unwrap_or_else(|_| "-infinity".to_string()),
            };
            (key, b.parse::<i64>().unwrap_or(i64::MAX))
        })
        .unwrap_or(("-infinity".to_string(), i64::MAX));
    let cur_key = cur_start;

    // Page through candidates until `limit` *visible* rows accumulate
    // (visibility is checked in Rust — direct/followers-only rows can
    // dominate a raw page) or candidates run out. Bounded to 10
    // batches so a pathological invisible tail can't spin forever;
    // the cursor always sits on a consumed candidate row, so the next
    // request makes progress even on an empty visible page.
    let mut visible_rows: Vec<(i64, Option<chrono::DateTime<chrono::Utc>>)> = Vec::new();
    let mut last_key = cur_key.clone();
    let mut last_id = cur_id;
    let mut has_more = false;
    for _ in 0..10 {
        let want = (limit + 1).saturating_sub(visible_rows.len() as i64);
        if want <= 0 {
            has_more = true;
            break;
        }
        let rows: Vec<(i64, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
            "SELECT s.id, e.start_time FROM statuses s \
             JOIN events e ON e.status_id = s.id \
             WHERE s.id = ANY($1) AND NOT s.tombstone \
               AND (e.start_time IS NULL OR (e.start_time >= $2 AND ($3::timestamptz IS NULL OR e.start_time <= $3))) \
               AND ($4 OR e.status <> 'cancelled') \
               AND ($5::text IS NULL OR $5 = ANY(e.categories)) \
               AND (COALESCE(e.start_time, 'infinity'::timestamptz), s.id) > ($6::timestamptz, $7) \
             ORDER BY COALESCE(e.start_time, 'infinity'::timestamptz) ASC, s.id ASC \
             LIMIT $8",
        )
        .bind(&ids)
        .bind(from)
        .bind(to)
        .bind(include_cancelled)
        .bind(&category)
        .bind(&last_key)
        .bind(last_id)
        // One extra row detects whether candidates remain.
        .bind(want + 1)
        .fetch_all(&state.db)
        .await?;
        let fetched = rows.len() as i64;
        let consumed = rows.iter().take(want.max(0) as usize).collect::<Vec<_>>();
        if consumed.is_empty() {
            break;
        }
        // Advance the scan cursor to the last consumed candidate.
        let (lid, lst) = consumed.last().unwrap();
        last_id = *lid;
        last_key = match lst {
            Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string(),
            None => "infinity".to_string(),
        };
        has_more = fetched > want;
        let batch_ids: Vec<i64> = consumed.iter().map(|(id, _)| *id).collect();
        let statuses = posts::load_many(&state.db, &batch_ids).await?;
        let batch: Vec<posts::StatusRow> = batch_ids
            .iter()
            .filter_map(|id| statuses.get(id).cloned())
            .collect();
        let visible = posts::visible_ids(&state.db, &batch, Some(viewer)).await?;
        for (id, st) in consumed {
            if visible.contains(id) {
                visible_rows.push((*id, *st));
            }
        }
        if !has_more {
            break;
        }
    }

    let page_full = visible_rows.len() as i64 > limit;
    let page: Vec<i64> = visible_rows
        .iter()
        .take(limit as usize)
        .map(|(id, _)| *id)
        .collect();
    let next_cursor = if page_full || has_more {
        // Cursor on the last *returned* row when the page is full;
        // otherwise on the last consumed candidate so an
        // all-invisible tail still advances.
        let (key, id) = if page_full {
            let (id, st) = visible_rows[limit as usize - 1];
            let key = match st {
                Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string(),
                None => "inf".to_string(),
            };
            (key, id)
        } else {
            let key = if last_key == "infinity" {
                "inf".to_string()
            } else {
                last_key.clone()
            };
            (key, last_id)
        };
        Some(format!("{key}|{id}"))
    } else {
        None
    };

    // `page` ids are already visibility-filtered above.
    let statuses = posts::load_many(&state.db, &page).await?;
    let page_rows: Vec<posts::StatusRow> = page
        .iter()
        .filter_map(|id| statuses.get(id).cloned())
        .collect();
    let out = posts::entity::status_entities(&ctx(&state), &page_rows, Some(viewer)).await?;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    if let Some(c) = next_cursor {
        let mut q = params.clone();
        q.insert("cursor".into(), c);
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(q.iter())
            .finish();
        let base = ctx(&state).base_url();
        if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
            "<{base}{}?{}>; rel=\"next\"",
            uri.path(),
            query
        )) {
            headers.insert(header::LINK, v);
        }
    }
    Ok((headers, Json(serde_json::json!(out))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_near_absent_is_none() {
        assert_eq!(parse_near(None).unwrap(), None);
        assert!(parse_near(Some(&String::new())).is_err());
    }

    #[test]
    fn parse_near_accepts_coords() {
        let raw = " 45.12 , 9.40 ".to_string();
        let (lat, lon) = parse_near(Some(&raw)).unwrap().unwrap();
        assert_eq!(lat, 45.12);
        assert_eq!(lon, 9.40);
    }

    #[test]
    fn parse_radius_km_defaults_clamps_and_rejects_non_finite() {
        assert_eq!(parse_radius_km(None).unwrap(), 50.0);
        assert_eq!(parse_radius_km(Some(&"12.5".to_string())).unwrap(), 12.5);
        assert_eq!(parse_radius_km(Some(&"0".to_string())).unwrap(), 0.1);
        assert_eq!(parse_radius_km(Some(&"9000".to_string())).unwrap(), 500.0);
        for bad in ["NaN", "inf", "-inf", "far"] {
            assert!(
                parse_radius_km(Some(&bad.to_string())).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    /// Great-circle distance in km (haversine form), for the box check.
    fn great_circle_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
        let dlon = (lon2 - lon1).to_radians();
        let h = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
        2.0 * 6371.0 * h.sqrt().min(1.0).asin()
    }

    #[test]
    fn near_lon_box_covers_every_in_radius_event() {
        // For each query latitude and radius, the largest longitude
        // offset of any event inside the radius (found by bisection
        // along the band) must fit inside the box. The query-latitude
        // box failed this poleward of ~50°.
        for qlat in [-75.0, -60.0, -30.0, 0.0, 30.0, 45.0, 60.0, 70.0, 80.0] {
            for radius in [10.0, 50.0, 200.0, 500.0] {
                let half = lon_half_width_deg(qlat, radius);
                let lat_delta = radius / 111.0;
                for k in -200..=200 {
                    let elat = qlat + lat_delta * (k as f64 / 200.0);
                    let (mut lo, mut hi) = (0.0_f64, 180.0_f64);
                    for _ in 0..60 {
                        let mid = (lo + hi) / 2.0;
                        if great_circle_km(qlat, 0.0, elat, mid) <= radius {
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    assert!(
                        lo <= half + 1e-9,
                        "qlat={qlat} radius={radius} elat={elat}: true {lo} > box {half}"
                    );
                }
            }
        }
    }

    #[test]
    fn parse_near_rejects_malformed_and_out_of_range() {
        for bad in ["45.12", "abc,def", "91,0", "0,181", "45.12,9.40,3"] {
            assert!(
                parse_near(Some(&bad.to_string())).is_err(),
                "{bad} should be rejected"
            );
        }
    }
}
