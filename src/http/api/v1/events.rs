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
        "local" | "federated" => {
            let vis = if scope == "federated" {
                "'public'"
            } else {
                "'public','local','unlisted'"
            };
            sqlx::query_scalar(&format!(
                "SELECT s.id FROM statuses s \
                 JOIN events e ON e.status_id = s.id \
                 WHERE s.object_type = 'Event' AND NOT s.tombstone \
                   AND s.local AND s.visibility IN ({vis})"
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
