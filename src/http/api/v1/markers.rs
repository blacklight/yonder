//! `/api/v1/markers` — Mastodon timeline markers: per-account
//! last-read positions for the `home` and `notifications` timelines.
//! Mobile apps (official Mastodon, Moshidon) read them on every
//! timeline/notifications load and write them on scroll.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::header,
    Json,
};
use serde_json::{json, Map, Value};
use sqlx::PgPool;

use crate::{
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// Marker timelines the schema accepts (mirrors `markers.timeline`'s
/// CHECK — keep in sync).
const TIMELINES: &[&str] = &["home", "notifications"];

/// `GET /api/v1/markers?timeline[]=home&timeline[]=notifications` —
/// last-read ids per requested timeline; unknown timelines are absent
/// from the response (Mastodon semantics).
#[utoipa::path(
    get,
    path = "/api/v1/markers",
    tag = "timelines",
    params(("timeline[]" = Option<Vec<String>>, Query, description = "home|notifications (default: both)")),
    responses(
        (status = 200, description = "Last-read positions keyed by timeline"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn get_markers(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    let query = request.uri().query().unwrap_or("");
    // `timeline[]` may repeat; also accept a bare `timeline` key.
    let requested: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k == "timeline[]" || k == "timeline")
        .map(|(_, v)| v.into_owned())
        .collect();
    let timelines: Vec<&str> = TIMELINES
        .iter()
        .copied()
        .filter(|t| requested.is_empty() || requested.iter().any(|r| r == t))
        .collect();

    let rows = sqlx::query_as::<_, (String, i64, i32, chrono::DateTime<chrono::Utc>)>(
        "SELECT timeline, last_read_id, version, updated_at \
         FROM markers WHERE account_id = $1 AND timeline = ANY($2)",
    )
    .bind(auth.account_id)
    .bind(&timelines)
    .fetch_all(&state.db)
    .await?;

    let mut out = Map::new();
    for (timeline, last_read_id, version, updated_at) in rows {
        out.insert(
            timeline,
            json!({
                "last_read_id": last_read_id.to_string(),
                "version": version,
                "updated_at": updated_at.to_rfc3339(),
            }),
        );
    }
    Ok(Json(Value::Object(out)))
}

/// `POST /api/v1/markers` — upsert last-read positions. Body is form
/// (`home[last_read_id]=…`, `notifications[last_read_id]=…`) or JSON
/// (`{"home": {"last_read_id": "…"}}`); Mastodon clients send both.
#[utoipa::path(
    post,
    path = "/api/v1/markers",
    tag = "timelines",
    responses(
        (status = 200, description = "The stored markers"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 422, description = "Bad timeline name or id", body = crate::error::ErrorBody),
    )
)]
pub async fn save_markers(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    let updates = parse_updates(request).await?;
    for (timeline, last_read_id) in &updates {
        sqlx::query(
            "INSERT INTO markers (account_id, timeline, last_read_id, version, updated_at) \
             VALUES ($1, $2, $3, 1, now()) \
             ON CONFLICT (account_id, timeline) DO UPDATE \
             SET last_read_id = EXCLUDED.last_read_id, \
                 version = markers.version + 1, \
                 updated_at = now()",
        )
        .bind(auth.account_id)
        .bind(*timeline)
        .bind(*last_read_id)
        .execute(&state.db)
        .await?;
    }
    get_markers_for(&state.db, auth.account_id).await
}

/// Shared read: everything stored for the account.
async fn get_markers_for(db: &PgPool, account_id: i64) -> Result<Json<Value>> {
    let rows = sqlx::query_as::<_, (String, i64, i32, chrono::DateTime<chrono::Utc>)>(
        "SELECT timeline, last_read_id, version, updated_at \
         FROM markers WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_all(db)
    .await?;
    let mut out = Map::new();
    for (timeline, last_read_id, version, updated_at) in rows {
        out.insert(
            timeline,
            json!({
                "last_read_id": last_read_id.to_string(),
                "version": version,
                "updated_at": updated_at.to_rfc3339(),
            }),
        );
    }
    Ok(Json(Value::Object(out)))
}

fn parse_id(timeline: &str, value: &str) -> Result<i64> {
    value
        .parse()
        .map_err(|_| Error::Validation(format!("{timeline}[last_read_id] must be an integer")))
}

/// Normalize the body into `(timeline, last_read_id)` pairs — form
/// `home[last_read_id]` first, JSON `{"home": {"last_read_id"}}`.
async fn parse_updates(request: Request) -> Result<Vec<(&'static str, i64)>> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;

    let mut updates = Vec::new();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        for (key, value) in url::form_urlencoded::parse(&bytes) {
            for timeline in TIMELINES {
                if key == format!("{timeline}[last_read_id]") || key == *timeline {
                    updates.push((*timeline, parse_id(timeline, &value)?));
                }
            }
        }
    } else if !bytes.is_empty() {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::BadRequest(format!("malformed JSON body: {e}")))?;
        for timeline in TIMELINES {
            let v = body.get(*timeline).and_then(|t| t.get("last_read_id"));
            if let Some(v) = v {
                let raw = v
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                    .ok_or_else(|| {
                        Error::Validation(format!("{timeline}.last_read_id must be an integer"))
                    })?;
                updates.push((*timeline, parse_id(timeline, &raw)?));
            }
        }
    }
    Ok(updates)
}
