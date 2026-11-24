//! Nicolex-compatible `/api/v1/pleroma/events*` adapter — the route
//! shapes pl-fe (mkljczk's Pleroma fork client) calls, translated onto
//! Yonder's status/event services. Nicolex params map onto the FEP-8a8e
//! authoring payload: `name`→`title`, `status`→description,
//! `banner_id`→media + `banner_media_id`, `start_time`/`end_time`/
//! `join_mode`/`timezone`→`event.*`.
//!
//! `location_id` refers to entries returned by
//! `/api/v1/pleroma/search/location`; it resolves only when
//! `[events.geocoding]` is configured — otherwise create/edit answer
//! 422 with a hint, matching the contract.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::header,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    events::{api as events_api, entity as events_entity, geocoding, validate::EventParams},
    http::{
        middleware::{
            auth::{AuthContext, MaybeAuth},
            ratelimit,
        },
        AppState,
    },
    posts::{self, create::StatusParams, PostContext},
    users::account,
    Error, Result,
};

/// Mounted from `pleroma::router` — paths carry the full `/api` prefix.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/pleroma/events",
            post(create).route_layer(ratelimit::layer(state, "status")),
        )
        .route("/api/v1/pleroma/events/joined_events", get(joined_events))
        .route(
            "/api/v1/pleroma/events/{id}",
            post(edit)
                .put(edit)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/pleroma/events/{id}/participations",
            get(participations),
        )
        .route(
            "/api/v1/pleroma/events/{id}/participation_requests",
            get(participation_requests),
        )
        .route(
            "/api/v1/pleroma/events/{id}/participation_requests/{account_id}/{decision}",
            post(decide_request).route_layer(ratelimit::layer(state, "events_rsvp")),
        )
        .route(
            "/api/v1/pleroma/events/{id}/join",
            post(join).route_layer(ratelimit::layer(state, "events_rsvp")),
        )
        .route(
            "/api/v1/pleroma/events/{id}/leave",
            post(leave).route_layer(ratelimit::layer(state, "events_rsvp")),
        )
        .route(
            "/api/v1/pleroma/events/{id}/ics",
            get(crate::http::api::v1::events::event_ics),
        )
        .route(
            "/api/v1/pleroma/search/location",
            get(search_location).route_layer(ratelimit::layer(state, "search")),
        )
}

// ---------------------------------------------------------------------------
// Param translation
// ---------------------------------------------------------------------------

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn i64_param(params: &Value, key: &str) -> Result<Option<i64>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| Error::Validation(format!("{key} must be an integer"))),
        Some(Value::String(s)) => s
            .parse()
            .map(Some)
            .map_err(|_| Error::Validation(format!("{key} must be an integer"))),
        _ => Err(Error::Validation(format!("{key} must be an integer"))),
    }
}

/// Async wrapper over the shared geocoder fetch
/// (`events::geocoding::geocode_get`).
async fn geocode_get(state: &AppState, url: String) -> Result<Vec<u8>> {
    geocoding::geocode_get(&state.config().events.geocoding, url).await
}

/// `location_id` → a `place` [`LocationParams`] via the configured
/// geocoder. `provider = "none"` → 422 with a hint (the contract's
/// documented degradation); the id is the Nominatim `place_id`
/// `search/location` emitted.
async fn location_from_id(
    state: &AppState,
    location_id: i64,
) -> Result<Option<crate::events::validate::LocationParams>> {
    let geo = &state.config().events.geocoding;
    if !matches!(geo.provider, crate::config::GeocodingProvider::Nominatim) {
        return Err(Error::Validation(
            "location_id requires events.geocoding to be configured; \
             send event locations inline or disable location_id"
                .to_string(),
        ));
    }
    let url = format!(
        "{}/lookup?place_id={location_id}&format=jsonv2&addressdetails=1",
        geo.nominatim_url.trim_end_matches('/')
    );
    let bytes = geocode_get(state, url).await?;
    let body: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Upstream(format!("geocoder lookup failed: {e}")))?;
    let Some(first) = body.first() else {
        return Err(Error::Validation("location_id not found".to_string()));
    };
    let mut loc = serde_json::to_value(geocoding::hit_from_nominatim(first))
        .map_err(|e| Error::Internal(format!("geocoder result: {e}")))?;
    loc["type"] = json!("place");
    serde_json::from_value(loc)
        .map(Some)
        .map_err(|e| Error::Validation(format!("location: {e}")))
}

/// Nicolex params → [`StatusParams`] for `post_type=event`.
/// `editing` merges absent `name`/`status` from the stored row so a
/// partial PUT doesn't wipe them; `event` is `None` when the body
/// carries no event fields at all (edit keeps the stored payload).
async fn status_params(
    state: &AppState,
    params: &Value,
    status_id: Option<i64>,
) -> Result<StatusParams> {
    let stored = match status_id {
        Some(id) => posts::load(&state.db, id).await?,
        None => None,
    };
    let name = str_param(params, "name")
        .map(str::to_string)
        .or_else(|| stored.as_ref().and_then(|s| s.title.clone()));
    let description = match params.get("status") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => String::new(),
        None => stored
            .as_ref()
            .map(|s| s.content.clone())
            .unwrap_or_default(),
        _ => return Err(Error::Validation("status must be a string".into())),
    };

    // Event fields — presence of any triggers the `event` payload.
    // On edit the stored event row is the merge base: only keys the
    // request actually carries overwrite it, so a partial PUT can't
    // wipe locations/categories/invitees/media or reset the timezone.
    let event_keys = [
        "start_time",
        "end_time",
        "timezone",
        "event_status",
        "status_type",
        "join_mode",
        "external_participation_url",
        "maximum_attendee_capacity",
        "attendees_visibility",
        "categories",
        "locations",
        "banner_media_id",
        "flyer_media_id",
        "invitees",
    ];
    let has_event_fields = event_keys.iter().any(|k| params.get(*k).is_some());
    let mut event = match status_id {
        Some(id) => crate::events::validate::params_from_stored(&state.db, id)
            .await?
            .unwrap_or_default(),
        None => EventParams::default(),
    };
    if has_event_fields || status_id.is_none() {
        if params.get("start_time").is_some() {
            event.start_time = str_param(params, "start_time").map(str::to_string);
        }
        if params.get("end_time").is_some() {
            event.end_time = str_param(params, "end_time").map(str::to_string);
        }
        // Nicolex carries the offset inside the RFC-3339 instants;
        // `timezone` is display metadata.
        if params.get("timezone").is_some() {
            event.timezone = str_param(params, "timezone").map(str::to_string);
        }
        if params.get("event_status").is_some() || params.get("status_type").is_some() {
            event.status = str_param(params, "event_status")
                .or_else(|| str_param(params, "status_type"))
                .map(str::to_string);
        }
        if params.get("join_mode").is_some() {
            event.join_mode = str_param(params, "join_mode").map(str::to_string);
        }
        if params.get("external_participation_url").is_some() {
            event.external_participation_url =
                str_param(params, "external_participation_url").map(str::to_string);
        }
        if params.get("maximum_attendee_capacity").is_some() {
            event.maximum_attendee_capacity = i64_param(params, "maximum_attendee_capacity")?;
        }
        if params.get("attendees_visibility").is_some() {
            event.attendees_visibility =
                str_param(params, "attendees_visibility").map(str::to_string);
        }
        if let Some(Value::Array(cats)) = params.get("categories") {
            event.categories = cats
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect();
        }
        if let Some(Value::Array(locs)) = params.get("locations") {
            event.locations = serde_json::from_value(Value::Array(locs.clone()))
                .map_err(|e| Error::Validation(format!("locations: {e}")))?;
        }
        if params.get("banner_media_id").is_some() {
            event.banner_media_id = i64_param(params, "banner_media_id")?;
        }
        if params.get("flyer_media_id").is_some() {
            event.flyer_media_id = i64_param(params, "flyer_media_id")?;
        }
        if let Some(Value::Array(ids)) = params.get("invitees") {
            event.invitees = ids.iter().filter_map(Value::as_i64).collect();
        }
        // `location_id` resolves through the geocoder into an inline
        // place appended to `locations`.
        if let Some(id) = i64_param(params, "location_id")? {
            if let Some(lp) = location_from_id(state, id).await? {
                event.locations.push(lp);
            }
        }
    }

    // `banner_id` is Nicolex's media param — it both attaches the
    // media and marks it as the event banner.
    let banner_id = i64_param(params, "banner_id")?;
    let media_ids: Option<Vec<i64>> = banner_id.map(|id| vec![id]);
    // `banner_media_id` (explicit event key) wins; otherwise a
    // `banner_id` in the request becomes the banner — including over
    // a stored one, since attaching new banner media implies it.
    if let Some(id) = banner_id {
        if params.get("banner_media_id").is_none() {
            event.banner_media_id = Some(id);
        }
    }

    Ok(StatusParams {
        status: description,
        content_type: str_param(params, "content_type").map(str::to_string),
        visibility: str_param(params, "visibility").map(str::to_string),
        language: str_param(params, "language").map(str::to_string),
        sensitive: params.get("sensitive").and_then(Value::as_bool),
        spoiler_text: str_param(params, "spoiler_text").map(str::to_string),
        in_reply_to_id: None,
        media_ids,
        poll: None,
        quote_id: i64_param(params, "quote_id")?,
        scheduled_at: str_param(params, "scheduled_at")
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc)),
        post_type: Some("event".to_string()),
        title: name,
        summary: None,
        cover_media_id: None,
        event: if has_event_fields || status_id.is_none() {
            Some(event)
        } else {
            None
        },
        event_invite_uri: None,
    })
}

async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    req: Request<Body>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let params = super::body_params(req).await?;
    let status_params = status_params(&state, &params, None).await?;
    let ctx = PostContext::new(&state).with_client_app(auth.app_name.as_deref());
    match posts::create::create_status(&ctx, &auth, &status_params, None).await? {
        posts::create::CreateOutcome::Published(entity)
        | posts::create::CreateOutcome::Scheduled(entity) => Ok(Json(entity)),
    }
}

async fn edit(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let params = super::body_params(req).await?;
    let status_params = status_params(&state, &params, Some(id)).await?;
    let ctx = PostContext::new(&state).with_client_app(auth.app_name.as_deref());
    let status = posts::create::edit_status(&ctx, &auth, id, &status_params).await?;
    Ok(Json(
        posts::entity::status_entity(&ctx, &status, Some(auth.account_id)).await?,
    ))
}

// ---------------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------------

/// Load an event status the viewer can see; 404 otherwise.
async fn load_event(
    state: &AppState,
    status_id: i64,
    viewer: Option<i64>,
) -> Result<posts::StatusRow> {
    let status = posts::load(&state.db, status_id)
        .await?
        .filter(posts::is_event)
        .ok_or_else(|| Error::NotFound("event".into()))?;
    if !posts::can_see(&state.db, &status, viewer).await? {
        return Err(Error::NotFound("event".into()));
    }
    Ok(status)
}

#[derive(Deserialize)]
struct JoinedQuery {
    state: Option<String>,
    #[serde(rename = "max_id")]
    max_id: Option<i64>,
    #[serde(rename = "since_id")]
    since_id: Option<i64>,
    #[serde(rename = "min_id")]
    min_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/pleroma/events/joined_events?state=` — the viewer's
/// event participations as a paginated Status list (id-DESC,
/// Mastodon `max_id`/`since_id`/`min_id` conventions).
async fn joined_events(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(q): Query<JoinedQuery>,
) -> Result<Response> {
    let limit = q.limit.unwrap_or(20).clamp(1, 40);
    let participation_state = q.state.as_deref().map(|s| match s {
        // pl-fe sends Mastodon-ish filters; normalize the common ones.
        "going" | "accepted" => "accepted",
        other => other,
    });
    // `state=all`/`null` means no filter; a concrete state filters.
    let state_filter = participation_state.filter(|s| *s != "all" && *s != "null");
    // `since_id` and `min_id` carry the same "id >" bound.
    let after = q.since_id.or(q.min_id);
    let rows: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM event_participations p \
         JOIN statuses s ON s.id = p.status_id \
         WHERE p.account_id = $1 AND s.object_type = 'Event' AND NOT s.tombstone \
           AND ($2::text IS NULL OR p.state = $2) \
           AND ($3::bigint IS NULL OR s.id < $3) \
           AND ($4::bigint IS NULL OR s.id > $4) \
         ORDER BY s.id DESC LIMIT $5",
    )
    .bind(auth.account_id)
    .bind(state_filter)
    .bind(q.max_id)
    .bind(after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    let has_more = rows.len() as i64 > limit;
    let page: Vec<i64> = rows.iter().take(limit as usize).copied().collect();
    let statuses = posts::load_many(&state.db, &page).await?;
    let mut statuses_vec: Vec<posts::StatusRow> = page
        .iter()
        .filter_map(|id| statuses.get(id).cloned())
        .collect();
    // `load_many` doesn't guarantee order — restore the query order.
    statuses_vec.sort_by_key(|s| std::cmp::Reverse(s.id));
    let ctx = PostContext::new(&state).with_client_app(auth.app_name.as_deref());
    let out = posts::entity::status_entities(&ctx, &statuses_vec, Some(auth.account_id)).await?;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let base = format!(
        "{}/api/v1/pleroma/events/joined_events",
        ctx.base_url().trim_end_matches('/')
    );
    let mut links = Vec::new();
    if let (Some(first), Some(last)) = (page.first(), page.last()) {
        if has_more {
            links.push(format!("<{base}?max_id={last}>; rel=\"next\""));
        }
        links.push(format!("<{base}?since_id={first}>; rel=\"prev\""));
    }
    if !links.is_empty() {
        if let Ok(v) = axum::http::HeaderValue::from_str(&links.join(", ")) {
            headers.insert(header::LINK, v);
        }
    }
    Ok((headers, Json(json!(out))).into_response())
}

/// `GET .../participations` — Account entities for going participants,
/// gated by `attendees_visibility` (same rule as the AP collection).
async fn participations(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let status = load_event(&state, id, viewer).await?;
    let bundle = events_entity::load_many(&state.db, &[id], viewer)
        .await?
        .remove(&id)
        .ok_or_else(|| Error::NotFound("event".into()))?;

    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT p.account_id, p.state FROM event_participations p \
         WHERE p.status_id = $1 AND p.role <> 'organizer' \
           AND p.state IN ('accepted','tentative') \
         ORDER BY p.created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    let is_org = bundle.viewer_is_organizer(viewer, status.account_id);
    let is_participant = rows.iter().any(|(aid, _)| Some(*aid) == viewer);

    use crate::events::model::AttendeesVisibility;
    let allowed = match bundle.row.attendees_visibility {
        AttendeesVisibility::Public => true,
        AttendeesVisibility::Participants => is_org || is_participant,
        AttendeesVisibility::Organizers => is_org,
    };
    if !allowed {
        return Ok(Json(json!([])));
    }
    let ids: Vec<i64> = rows.iter().map(|(aid, _)| *aid).collect();
    let accounts = account::load_many(&state.db, &ids).await?;
    let entities = account::account_entities(
        &state.db,
        &state.config(),
        &crate::activitypub::federation_base_url(state.config().as_ref()),
        &accounts,
    )
    .await?;
    Ok(Json(json!(entities)))
}

/// `GET .../participation_requests` — pending joins with their
/// message; organizer-only.
async fn participation_requests(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let status = load_event(&state, id, Some(auth.account_id)).await?;
    if !events_api::is_organizer(&state.db, &status, auth.account_id).await? {
        return Err(Error::Forbidden("only organizers can list requests"));
    }
    let rows = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT account_id, participation_message FROM event_participations \
         WHERE status_id = $1 AND state = 'pending' AND role <> 'organizer' \
         ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|(aid, _)| *aid).collect();
    let accounts = account::load_many(&state.db, &ids).await?;
    let entities = account::account_entities(
        &state.db,
        &state.config(),
        &crate::activitypub::federation_base_url(state.config().as_ref()),
        &accounts,
    )
    .await?;
    let by_id: std::collections::HashMap<i64, Value> =
        accounts.iter().map(|a| a.id).zip(entities).collect();
    let out: Vec<Value> = rows
        .into_iter()
        .map(|(aid, msg)| {
            json!({
                "account": by_id.get(&aid).cloned().unwrap_or(Value::Null),
                "participation_message": msg,
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

/// `POST .../participation_requests/{account_id}/{authorize|reject}`.
async fn decide_request(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path((id, account_id, decision)): Path<(i64, i64, String)>,
) -> Result<Json<Value>> {
    let approve = match decision.as_str() {
        "authorize" => true,
        "reject" => false,
        _ => {
            return Err(Error::Validation(
                "decision must be authorize or reject".to_string(),
            ))
        }
    };
    events_api::decide_request(
        &PostContext::new(&state),
        auth.account_id,
        id,
        account_id,
        approve,
    )
    .await?;
    Ok(Json(json!({"ok": true})))
}

/// `POST .../join` — `{participation_message?}`; maps onto the RSVP
/// state machine (`invite_response` events send `Accept`, join-mode
/// events `Join`, pending joins on restricted events queue).
async fn join(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>> {
    let message = body
        .as_ref()
        .and_then(|b| b.get("participation_message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let result = events_api::rsvp(
        &PostContext::new(&state),
        auth.account_id,
        id,
        &events_api::RsvpBody {
            state: "accepted".to_string(),
            message,
        },
    )
    .await?;
    Ok(Json(json!({"ok": true, "participation": result})))
}

/// `POST .../leave`.
async fn leave(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let result = events_api::rsvp(
        &PostContext::new(&state),
        auth.account_id,
        id,
        &events_api::RsvpBody {
            state: "none".to_string(),
            message: None,
        },
    )
    .await?;
    Ok(Json(json!({"ok": true, "participation": result})))
}

/// `GET /api/v1/pleroma/search/location?q=` — Nominatim proxy when
/// `[events.geocoding]` is configured; `[]` otherwise.
async fn search_location(
    State(state): State<Arc<AppState>>,
    _auth: AuthContext,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>> {
    let geo = &state.config().events.geocoding;
    if !matches!(geo.provider, crate::config::GeocodingProvider::Nominatim) {
        return Ok(Json(json!([])));
    }
    let query = q.get("q").cloned().unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(json!([])));
    }
    let url = format!(
        "{}/search?format=jsonv2&addressdetails=1&limit=5&q={}",
        geo.nominatim_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let bytes = geocode_get(&state, url).await?;
    let results: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Upstream(format!("geocoder search failed: {e}")))?;
    // Nicolex clients consume `{id, name, latitude, longitude, ...}` —
    // `id` is the Nominatim `place_id` `location_id` resolves back.
    let out: Vec<Value> = results
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get("place_id"),
                "name": r.get("name").or_else(|| r.get("display_name")),
                "latitude": r.get("lat"),
                "longitude": r.get("lon"),
                "address": r.get("address"),
                "display_name": r.get("display_name"),
            })
        })
        .collect();
    Ok(Json(json!(out)))
}
