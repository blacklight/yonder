//! `/api/v1/push/subscription` — Mastodon WebPush subscription CRUD
//! over the existing `web_push_subscriptions` table (migration 0007:
//! `account_id`-keyed, `endpoint` globally unique, `data` JSONB).
//! Mobile apps (official Mastodon, Moshidon via UnifiedPush) register
//! on login; `server_key` is the instance VAPID public key
//! (`http.vapid_public_key` — `yonder webpush generate-keys`).
//! Subscriptions are stored now; actual push delivery lands with the
//! WebPush epic — the endpoint contract is already correct.
//!
//! `data` layout: `{"alerts": {<type>: bool}, "policy": "all|…"}`.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::header,
    Json,
};
use serde_json::{json, Map, Value};
use sqlx::Row;

use crate::{
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// Alert types Mastodon's `data[alerts]` object knows; `reaction` is
/// the Pleroma/`emoji_reaction` extension Moshidon sends.
const ALERT_TYPES: &[&str] = &[
    "follow",
    "follow_request",
    "favourite",
    "reblog",
    "mention",
    "poll",
    "status",
    "update",
    "quote",
    "admin.sign_up",
    "admin.report",
    "reaction",
];

const POLICIES: &[&str] = &["all", "followed", "follower", "none"];

fn server_key(state: &AppState) -> Value {
    match &state.config().http.vapid_public_key {
        Some(k) if !k.is_empty() => json!(k),
        _ => Value::Null,
    }
}

fn entity(state: &AppState, r: &sqlx::postgres::PgRow) -> Value {
    let data: Value = r.get("data");
    json!({
        "id": r.get::<i64, _>("id").to_string(),
        "endpoint": r.get::<String, _>("endpoint"),
        "alerts": data.get("alerts").cloned().unwrap_or(json!({})),
        "policy": data.get("policy").cloned().unwrap_or(json!("all")),
        "server_key": server_key(state),
    })
}

const SUB_COLS: &str = "id, endpoint, data";

/// The caller's newest subscription, if any.
async fn current(db: &sqlx::PgPool, account_id: i64) -> Result<Option<sqlx::postgres::PgRow>> {
    sqlx::query(&format!(
        "SELECT {SUB_COLS} FROM web_push_subscriptions \
         WHERE account_id = $1 ORDER BY updated_at DESC LIMIT 1"
    ))
    .bind(account_id)
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// `GET /api/v1/push/subscription` — the caller's current
/// subscription; 404 when none was registered (Mastodon semantics).
#[utoipa::path(
    get,
    path = "/api/v1/push/subscription",
    tag = "push",
    responses(
        (status = 200, description = "The WebPushSubscription entity"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No subscription", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "push")?;
    let row = current(&state.db, auth.account_id)
        .await?
        .ok_or_else(|| Error::NotFound("subscription".into()))?;
    Ok(Json(entity(&state, &row)))
}

/// Normalize an alerts map: unknown keys dropped, non-bool coerced
/// through Mastodon's truthy/falsey rules (absent key = unchanged on
/// update, default true on create).
fn clean_alerts(v: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if !ALERT_TYPES.contains(&k.as_str()) {
                continue;
            }
            let b = match val {
                Value::Bool(b) => *b,
                Value::String(s) => s != "false" && s != "0",
                Value::Number(n) => n.as_i64() != Some(0),
                _ => true,
            };
            out.insert(k.clone(), json!(b));
        }
    }
    out
}

/// Parsed POST/PUT body: endpoint + keys for create, alerts/policy
/// for both. Form shape: `subscription[endpoint]`,
/// `subscription[keys][p256dh]`, `subscription[keys][auth]`,
/// `data[alerts][follow]=true`, `policy=all`. JSON shape: nested
/// objects.
#[derive(Default)]
struct SubParams {
    endpoint: Option<String>,
    p256dh: Option<String>,
    auth_key: Option<String>,
    alerts: Option<Value>,
    policy: Option<String>,
    endpoint_id: Option<i64>,
}

async fn parse_body(request: Request) -> Result<SubParams> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    let mut out = SubParams::default();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let mut alerts = Map::new();
        for (k, v) in url::form_urlencoded::parse(&bytes) {
            match k.as_ref() {
                "subscription[endpoint]" => out.endpoint = Some(v.into_owned()),
                "subscription[keys][p256dh]" => out.p256dh = Some(v.into_owned()),
                "subscription[keys][auth]" => out.auth_key = Some(v.into_owned()),
                "policy" => out.policy = Some(v.into_owned()),
                "id" => out.endpoint_id = v.parse().ok(),
                _ => {
                    if let Some(name) = k
                        .strip_prefix("data[alerts][")
                        .and_then(|s| s.strip_suffix(']'))
                    {
                        alerts.insert(name.to_string(), json!(v.into_owned()));
                    }
                }
            }
        }
        if !alerts.is_empty() {
            out.alerts = Some(Value::Object(alerts));
        }
    } else if !bytes.is_empty() {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::BadRequest(format!("malformed JSON body: {e}")))?;
        let sub = body.get("subscription").cloned().unwrap_or(Value::Null);
        out.endpoint = sub
            .get("endpoint")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let keys = sub.get("keys").cloned().unwrap_or(Value::Null);
        out.p256dh = keys
            .get("p256dh")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.auth_key = keys
            .get("auth")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.alerts = body.get("data").and_then(|d| d.get("alerts")).cloned();
        out.policy = body
            .get("policy")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.endpoint_id = body.get("id").and_then(|v| v.as_i64());
    }
    Ok(out)
}

fn validate_policy(policy: Option<&str>) -> Result<Option<String>> {
    match policy {
        None => Ok(None),
        Some(p) if POLICIES.contains(&p) => Ok(Some(p.to_string())),
        Some(p) => Err(Error::Validation(format!("unknown policy '{p}'"))),
    }
}

/// `POST /api/v1/push/subscription` — register (or re-register, keyed
/// on the globally-unique endpoint) a WebPush endpoint. A second
/// registration for the same endpoint updates keys/alerts —
/// Mastodon's upsert semantics; an endpoint owned by another account
/// is re-keyed to this one.
#[utoipa::path(
    post,
    path = "/api/v1/push/subscription",
    tag = "push",
    responses(
        (status = 200, description = "The stored WebPushSubscription"),
        (status = 422, description = "Missing endpoint/keys or bad policy", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "push")?;
    let params = parse_body(request).await?;
    let endpoint = params
        .endpoint
        .filter(|e| !e.is_empty())
        .ok_or_else(|| Error::Validation("subscription[endpoint] is required".into()))?;
    let p256dh = params
        .p256dh
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Error::Validation("subscription[keys][p256dh] is required".into()))?;
    let auth_key = params
        .auth_key
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Error::Validation("subscription[keys][auth] is required".into()))?;
    let policy = validate_policy(params.policy.as_deref())?.unwrap_or_else(|| "all".into());
    // Mastodon defaults every alert on unless told otherwise.
    let mut alerts = Map::new();
    for t in ALERT_TYPES {
        alerts.insert((*t).to_string(), json!(true));
    }
    if let Some(v) = &params.alerts {
        alerts.extend(clean_alerts(v));
    }
    let data = json!({"alerts": Value::Object(alerts), "policy": policy});
    let row = sqlx::query(&format!(
        "INSERT INTO web_push_subscriptions (account_id, endpoint, p256dh, auth, data) \
         VALUES ($1,$2,$3,$4,$5) \
         ON CONFLICT (endpoint) DO UPDATE \
         SET account_id = EXCLUDED.account_id, p256dh = EXCLUDED.p256dh, \
             auth = EXCLUDED.auth, data = EXCLUDED.data, updated_at = now() \
         RETURNING {SUB_COLS}"
    ))
    .bind(auth.account_id)
    .bind(&endpoint)
    .bind(&p256dh)
    .bind(&auth_key)
    .bind(&data)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(entity(&state, &row)))
}

/// `PUT /api/v1/push/subscription` — update alerts/policy of the
/// caller's current subscription (Mastodon also accepts `id` to name
/// it; we honor it when it matches).
#[utoipa::path(
    put,
    path = "/api/v1/push/subscription",
    tag = "push",
    responses(
        (status = 200, description = "The updated WebPushSubscription"),
        (status = 404, description = "No subscription", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "push")?;
    let params = parse_body(request).await?;
    let existing = current(&state.db, auth.account_id)
        .await?
        .ok_or_else(|| Error::NotFound("subscription".into()))?;
    if let Some(id) = params.endpoint_id {
        if id != existing.get::<i64, _>("id") {
            return Err(Error::NotFound("subscription".into()));
        }
    }
    let policy = match validate_policy(params.policy.as_deref())? {
        Some(p) => p,
        None => existing
            .get::<Value, _>("data")
            .get("policy")
            .and_then(|v| v.as_str())
            .unwrap_or("all")
            .to_string(),
    };
    let mut alerts = existing
        .get::<Value, _>("data")
        .get("alerts")
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(v) = &params.alerts {
        alerts.extend(clean_alerts(v));
    }
    let data = json!({"alerts": Value::Object(alerts), "policy": policy});
    let row = sqlx::query(&format!(
        "UPDATE web_push_subscriptions SET data = $2, updated_at = now() \
         WHERE id = $1 RETURNING {SUB_COLS}"
    ))
    .bind(existing.get::<i64, _>("id"))
    .bind(&data)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(entity(&state, &row)))
}

/// `DELETE /api/v1/push/subscription` — drop the caller's
/// subscription(s); idempotent 200 like Mastodon.
#[utoipa::path(
    delete,
    path = "/api/v1/push/subscription",
    tag = "push",
    responses((status = 200, description = "Deleted"))
)]
pub async fn delete_subscription(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "push")?;
    // Optional endpoint in the body narrows the delete; absent → all
    // of the caller's subscriptions.
    let params = parse_body(request).await.unwrap_or_default();
    match params.endpoint {
        Some(e) => {
            sqlx::query(
                "DELETE FROM web_push_subscriptions WHERE account_id = $1 AND endpoint = $2",
            )
            .bind(auth.account_id)
            .bind(&e)
            .execute(&state.db)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM web_push_subscriptions WHERE account_id = $1")
                .bind(auth.account_id)
                .execute(&state.db)
                .await?;
        }
    }
    Ok(Json(json!({})))
}
