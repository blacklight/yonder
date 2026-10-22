//! POST inbox endpoints — the route side of the inbound pipeline.
//!
//! `dispatch_multi` never sees POSTs: these handlers do the cheap,
//! fast work inline — JSON validation, domain-block checks and an
//! `activities` dedupe row — then hand off to the `Federation` queue
//! and answer `202`. Signature verification and processing happen in
//! the worker ([`super::incoming`]), so bursts can't wedge the HTTP
//! runtime and nothing is lost on restart.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::http::AppState;
use crate::jobs::JobKind;

use super::moderation;

/// Body cap matching pubby's `MAX_INBOX_BODY`.
const MAX_INBOX_BODY: usize = 8 * 1024 * 1024;

fn json_resp(status: StatusCode, body: &Value) -> Response {
    (status, Json(body.clone())).into_response()
}

fn err(status: StatusCode, msg: &str) -> Response {
    json_resp(status, &json!({"error": msg}))
}

/// Every `actor` reference in an activity — the field may be a string,
/// an embedded object, or an array of either (multi-actor activities).
fn actor_urls(activity: &Value) -> Vec<String> {
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(m) => m.get("id").and_then(|i| i.as_str()).map(String::from),
            _ => None,
        }
    }
    match activity.get("actor") {
        Some(Value::Array(list)) => list.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => Vec::new(),
    }
}

/// `POST /ap/inbox` — the shared inbox; the worker resolves the
/// addressed actor from the activity's recipients.
pub async fn post_shared(State(state): State<Arc<AppState>>, req: Request) -> Response {
    handle(state, None, req).await
}

/// `POST /ap/users/{username}/inbox` — the per-user inbox; the worker
/// resolves the named actor directly.
pub async fn post_per_user(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    req: Request,
) -> Response {
    handle(state, Some(username), req).await
}

/// Validate → block-check → dedupe → enqueue → 202.
async fn handle(state: Arc<AppState>, username: Option<String>, req: Request) -> Response {
    if state.ap.is_none() {
        return err(StatusCode::NOT_FOUND, "federation disabled");
    }
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let headers: HashMap<String, String> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let body = match to_bytes(body, MAX_INBOX_BODY).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "inbox body too large"),
    };
    let activity: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid JSON"),
    };

    // Domain blocks are cheap (indexed) and checked at the edge so a
    // suspended domain doesn't even enqueue. `actor` may be a string,
    // an embedded object, or an array of either — check every element.
    let actor_urls = actor_urls(&activity);
    for actor_url in &actor_urls {
        if let Some(domain) = moderation::actor_domain(actor_url) {
            match moderation::is_blocked(&state.db, &domain).await {
                Ok(true) => return err(StatusCode::FORBIDDEN, "domain is suspended"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, domain, "domain-block check failed; allowing");
                }
            }
        }
    }

    // Dedupe: one `local=false` row per activity_id. Activities without
    // an `id` dedupe on their body hash.
    let activity_id = activity
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("urn:sha256:{}", hex::encode(Sha256::digest(&body))));
    let activity_type = activity
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let inserted: Option<i64> = match sqlx::query_scalar(
        "INSERT INTO activities (activity_id, type, actor_id, object, local) \
         VALUES ($1,$2,$3,$4,false) ON CONFLICT (activity_id) DO NOTHING \
         RETURNING id",
    )
    .bind(&activity_id)
    .bind(&activity_type)
    .bind(actor_urls.first().cloned().unwrap_or_default())
    .bind(&activity)
    .fetch_optional(&state.db)
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "inbox dedupe insert failed");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "storage error");
        }
    };
    if inserted.is_none() {
        // Already seen — acknowledge without re-enqueueing.
        return json_resp(StatusCode::ACCEPTED, &json!({"status": "ok"}));
    }

    // `body_hex` carries the raw request body — pubby needs the exact
    // bytes to verify HTTP-signature digests in the worker.
    let payload = json!({
        "activity": activity,
        "activity_id": activity_id,
        "username": username,
        "method": "POST",
        "path": path,
        "headers": headers,
        "body_hex": hex::encode(&body),
    });
    match state
        .dispatcher
        .enqueue(JobKind::FederationIncoming, payload)
        .await
    {
        Ok(_) => json_resp(StatusCode::ACCEPTED, &json!({"status": "ok"})),
        Err(e) => {
            tracing::error!(error = %e, "inbox enqueue failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "enqueue failed")
        }
    }
}
