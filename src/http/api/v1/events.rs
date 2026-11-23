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
};

use crate::{
    events::{entity, ics},
    http::{middleware::auth::MaybeAuth, AppState},
    posts, Error, Result,
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
