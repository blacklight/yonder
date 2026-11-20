//! ActivityPub Client-to-Server — `POST /ap/users/{username}/outbox`.
//!
//! Authenticated writes for AP-native clients (Fedifox and other
//! C2S implementations): the bearer token resolves to the account
//! named in the path, and the posted activity runs through the same
//! pipelines the REST API uses — `favourite`/`reblog`/`follow`/
//! `create` — so federation fan-out, notifications and timelines
//! behave identically to an API call.
//!
//! Supported types: `Like`, `Announce`, `Follow`, `Create` (note
//! types) and `Undo` for the first three. The response is `201` with
//! a `Location` header and `{"id": …}` body carrying the issued
//! activity id — the value clients record for later `Undo`s.

use std::sync::Arc;

use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes,
    http::middleware::auth::AuthContext,
    posts::{self, create, PostContext, StatusRow},
    social::follows,
    Error, Result,
};

/// Outbox bodies are single activities — far below the inbox cap.
const MAX_BODY: usize = 256 * 1024;

/// `POST /ap/users/{username}/outbox` — the actor's C2S outbox.
pub async fn post_outbox(
    State(state): State<Arc<crate::http::AppState>>,
    Path(username): Path<String>,
    auth: AuthContext,
    req: Request,
) -> Result<Response> {
    if state.ap.is_none() {
        return Err(Error::NotFound("outbox".into()));
    }
    let config = state.config();
    scopes::require_scope(config.as_ref(), &auth, "write")?;

    // The token must belong to the actor whose outbox this is.
    let (owner, actor_id): (String, String) =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1")
            .bind(auth.account_id)
            .fetch_one(&state.db)
            .await?;
    if !owner.eq_ignore_ascii_case(&username) {
        return Err(Error::Forbidden("token does not belong to this actor"));
    }

    // `application/ld+json` isn't an axum `Json` content type — read
    // the raw body and parse it ourselves (same as the inbox).
    let body = to_bytes(req.into_body(), MAX_BODY)
        .await
        .map_err(|_| Error::BadRequest("outbox body too large".into()))?;
    let activity: Value =
        serde_json::from_slice(&body).map_err(|_| Error::BadRequest("invalid JSON".into()))?;

    let kind = activity
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // A present `actor` must be us — C2S activities are always
    // attributed to the outbox owner.
    if let Some(actor) = crate::interactions::object_uri(activity.get("actor")) {
        if actor != actor_id {
            return Err(Error::BadRequest(
                "actor does not match the outbox owner".into(),
            ));
        }
    }

    let pctx = PostContext::new(&state);
    let id = dispatch(&state, &pctx, &auth, &owner, &actor_id, &kind, &activity).await?;

    let mut resp = Json(json!({ "id": id, "type": kind, "actor": actor_id })).into_response();
    *resp.status_mut() = StatusCode::CREATED;
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(header::LOCATION, v);
    }
    Ok(resp)
}

async fn dispatch(
    state: &Arc<crate::http::AppState>,
    pctx: &PostContext,
    auth: &AuthContext,
    owner: &str,
    actor_id: &str,
    kind: &str,
    activity: &Value,
) -> Result<String> {
    match kind {
        "Like" => like(pctx, auth, actor_id, activity).await,
        "Announce" => announce(pctx, auth, actor_id, activity).await,
        "Follow" => follow(state, pctx, auth, actor_id, activity).await,
        "Create" => create_note(pctx, auth, owner, actor_id, activity).await,
        "Undo" => undo(state, pctx, auth, actor_id, activity).await,
        _ => Err(Error::BadRequest(format!(
            "unsupported activity type {kind:?}"
        ))),
    }
}

/// Resolve an AP object URI to a local `statuses` row, fetching and
/// materializing it first when federation knows how.
async fn resolve_status(pctx: &PostContext, uri: &str) -> Result<StatusRow> {
    if let Some(status) = posts::load_by_uri(&pctx.db, uri).await? {
        return if status.tombstone {
            Err(Error::NotFound("status".into()))
        } else {
            Ok(status)
        };
    }
    let Some(ap) = &pctx.ap else {
        return Err(Error::NotFound("status".into()));
    };
    if let Some(fetched) = super::lookup::fetch_remote_object(ap, uri).await? {
        crate::search::resolve::materialize_remote(&pctx.db, ap, &fetched.url, &fetched.doc)
            .await?;
    }
    posts::load_by_uri(&pctx.db, uri)
        .await?
        .filter(|s| !s.tombstone)
        .ok_or_else(|| Error::NotFound("status".into()))
}

/// The URI an activity's `object` points at (string or `{id}` shape).
fn object_uri(v: Option<&Value>) -> Result<String> {
    crate::interactions::object_uri(v)
        .ok_or_else(|| Error::BadRequest("activity has no object".into()))
}

// ---------------------------------------------------------------------------
// Like / Announce
// ---------------------------------------------------------------------------

/// `Like` — favourite the target status; returns the issued activity
/// id (`favourites.activity_uri`, minted locally for local targets).
async fn like(
    pctx: &PostContext,
    auth: &AuthContext,
    actor_id: &str,
    activity: &Value,
) -> Result<String> {
    let uri = object_uri(activity.get("object"))?;
    let status = resolve_status(pctx, &uri).await?;
    crate::interactions::favourite::favourite(pctx, auth, status.id).await?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT activity_uri FROM favourites WHERE account_id = $1 AND status_id = $2",
    )
    .bind(auth.account_id)
    .bind(status.id)
    .fetch_one(&pctx.db)
    .await?;
    match stored {
        Some(id) => Ok(id),
        None => {
            // A like on a local status publishes nothing — mint an id
            // anyway so the client can `Undo` it later.
            let id = crate::interactions::new_activity_id(actor_id);
            sqlx::query(
                "UPDATE favourites SET activity_uri = $3 \
                 WHERE account_id = $1 AND status_id = $2",
            )
            .bind(auth.account_id)
            .bind(status.id)
            .bind(&id)
            .execute(&pctx.db)
            .await?;
            Ok(id)
        }
    }
}

/// `Announce` — boost the target; the boost row's `uri` is the issued
/// Announce activity id.
async fn announce(
    pctx: &PostContext,
    auth: &AuthContext,
    actor_id: &str,
    activity: &Value,
) -> Result<String> {
    let uri = object_uri(activity.get("object"))?;
    let status = resolve_status(pctx, &uri).await?;
    let boost = crate::interactions::boost::reblog(pctx, auth, status.id, None).await?;
    Ok(boost
        .uri
        .clone()
        .unwrap_or_else(|| crate::interactions::new_activity_id(actor_id)))
}

// ---------------------------------------------------------------------------
// Follow
// ---------------------------------------------------------------------------

/// `Follow` — follow the target actor. The issued id is the published
/// Follow's id when one exists (remote targets), else a synthetic
/// `{actor}#follows/{target}` id the `Undo` handler resolves directly.
async fn follow(
    state: &Arc<crate::http::AppState>,
    pctx: &PostContext,
    auth: &AuthContext,
    actor_id: &str,
    activity: &Value,
) -> Result<String> {
    let uri = object_uri(activity.get("object"))?;
    let target_id = follows::follow_by_uri(state, auth.account_id, &uri).await?;

    // The pending request stores the wire Follow (with its id); an
    // already-accepted remote follow leaves the published activity in
    // `activities`. Local targets have neither.
    let pending: Option<String> = sqlx::query_scalar(
        "SELECT activity->>'id' FROM follow_requests \
         WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(auth.account_id)
    .bind(target_id)
    .fetch_optional(&pctx.db)
    .await?
    .flatten();
    let published: Option<String> = match pending {
        Some(id) => Some(id),
        None => {
            sqlx::query_scalar(
                "SELECT activity_id FROM activities \
                 WHERE local AND type = 'Follow' AND actor_id = $1 \
                   AND object->>'object' = $2 \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(actor_id)
            .bind(&uri)
            .fetch_optional(&pctx.db)
            .await?
        }
    };
    Ok(published.unwrap_or_else(|| format!("{actor_id}#follows/{target_id}")))
}

// ---------------------------------------------------------------------------
// Create (Note/Article/…)
// ---------------------------------------------------------------------------

/// Pick a language-tagged string: a bare string, `{"en": …}`, or the
/// first string value of a `contentMap`-style object.
fn lang_pick(v: &Value) -> Option<&str> {
    if let Some(s) = v.as_str() {
        return Some(s);
    }
    let m = v.as_object()?;
    m.get("en")
        .and_then(Value::as_str)
        .or_else(|| m.values().find_map(Value::as_str))
}

/// Flatten an AS2 `content`/`contentMap` member to plain text.
fn content_text(obj: &Value) -> String {
    let html = obj
        .get("content")
        .and_then(lang_pick)
        .or_else(|| obj.get("contentMap").and_then(lang_pick))
        .unwrap_or_default();
    posts::render::html_to_text(html)
}

/// `to`/`cc` audience strings, flattened from string/object/array forms.
fn audience(activity: &Value, obj: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for v in [
        activity.get("to"),
        activity.get("cc"),
        obj.get("to"),
        obj.get("cc"),
    ]
    .into_iter()
    .flatten()
    {
        let items: Vec<&Value> = match v {
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        for item in items {
            if let Some(s) = item.as_str() {
                out.push(s.to_string());
            } else if let Some(id) = item.get("id").and_then(Value::as_str) {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn audience_contains(audience: &[String], uri: &str) -> bool {
    audience.iter().any(|a| a == uri)
}

/// `Create` — publish a note/article as a normal status.
async fn create_note(
    pctx: &PostContext,
    auth: &AuthContext,
    owner: &str,
    actor_id: &str,
    activity: &Value,
) -> Result<String> {
    let obj = activity
        .get("object")
        .filter(|o| o.is_object())
        .ok_or_else(|| Error::BadRequest("Create has no object".into()))?;
    let otype = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if !posts::remote::NOTE_TYPES.contains(&otype) {
        return Err(Error::BadRequest(format!(
            "unsupported object type {otype:?}"
        )));
    }

    let audience = audience(activity, obj);
    let visibility = if audience_contains(&audience, pubby_sdk::AS_PUBLIC) {
        "public"
    } else if audience_contains(&audience, &format!("{actor_id}/followers")) {
        "private"
    } else {
        "direct"
    };

    let in_reply_to_id = match obj.get("inReplyTo") {
        Some(v) => {
            let uris: Vec<String> = match v {
                Value::Array(a) => a
                    .iter()
                    .filter_map(|i| {
                        i.as_str()
                            .map(String::from)
                            .or_else(|| crate::interactions::object_uri(Some(i)))
                    })
                    .collect(),
                _ => crate::interactions::object_uri(Some(v))
                    .into_iter()
                    .collect(),
            };
            match uris.first() {
                Some(uri) => Some(resolve_status(pctx, uri).await?.id),
                None => None,
            }
        }
        None => None,
    };

    let html_to_plain = |v: Option<&Value>| -> Option<String> {
        v.and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("en").and_then(Value::as_str).map(str::to_string))
        })
        .map(|h| posts::render::html_to_text(&h))
        .filter(|s| !s.trim().is_empty())
    };
    let spoiler_text =
        html_to_plain(obj.get("summary")).map(|s| s.chars().take(2000).collect::<String>());

    let (post_type, title, summary) = if otype == "Article" {
        (
            Some("article".to_string()),
            obj.get("name")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(500).collect::<String>()),
            html_to_plain(obj.get("summary")).map(|s| s.chars().take(2000).collect()),
        )
    } else {
        (None, None, None)
    };

    let params = create::StatusParams {
        status: content_text(obj),
        content_type: Some("text/plain".to_string()),
        visibility: Some(visibility.to_string()),
        language: None,
        sensitive: Some(
            obj.get("sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        spoiler_text,
        in_reply_to_id,
        media_ids: None,
        poll: None,
        quote_id: None,
        scheduled_at: None,
        post_type,
        title,
        summary,
        cover_media_id: None,
    };
    let validated = create::validate(pctx, auth, &params, None).await?;
    let status = create::publish(pctx, auth.account_id, &validated, None).await?;
    Ok(status
        .uri
        .clone()
        .unwrap_or_else(|| pctx.status_urls(owner, status.id).0))
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// `Undo` — retract a previously issued `Like`/`Announce`/`Follow`.
/// Resolution order: the interaction tables keyed by the recorded
/// activity id, then the `activities` ledger for published ones.
async fn undo(
    state: &Arc<crate::http::AppState>,
    pctx: &PostContext,
    auth: &AuthContext,
    actor_id: &str,
    activity: &Value,
) -> Result<String> {
    let id = object_uri(activity.get("object"))?;

    // Like — `favourites.activity_uri` is the issued Like id.
    let liked_status: Option<i64> = sqlx::query_scalar(
        "SELECT status_id FROM favourites \
         WHERE activity_uri = $1 AND account_id = $2",
    )
    .bind(&id)
    .bind(auth.account_id)
    .fetch_optional(&pctx.db)
    .await?;
    if let Some(status_id) = liked_status {
        crate::interactions::favourite::unfavourite(pctx, auth, status_id).await?;
        return Ok(id);
    }

    // Announce — the boost row's `uri` is the issued Announce id.
    let boost_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM statuses \
         WHERE uri = $1 AND account_id = $2 \
           AND reblog_of_id IS NOT NULL AND NOT tombstone",
    )
    .bind(&id)
    .bind(auth.account_id)
    .fetch_optional(&pctx.db)
    .await?;
    if let Some(boost_id) = boost_id {
        let boost = posts::load(&pctx.db, boost_id)
            .await?
            .ok_or_else(|| Error::NotFound("boost".into()))?;
        crate::interactions::boost::unreblog_row(pctx, &boost).await?;
        return Ok(id);
    }

    // Follow — pending requests keep the wire Follow; accepted ones
    // leave the published activity in the `activities` ledger.
    let pending_target: Option<i64> = sqlx::query_scalar(
        "SELECT target_account_id FROM follow_requests \
         WHERE account_id = $1 AND activity->>'id' = $2",
    )
    .bind(auth.account_id)
    .bind(&id)
    .fetch_optional(&pctx.db)
    .await?;
    if let Some(target_id) = pending_target {
        follows::unfollow(state, auth.account_id, target_id).await?;
        return Ok(id);
    }

    // Published Follow — the ledger row names the target actor.
    let follow_target: Option<Option<String>> = sqlx::query_scalar(
        "SELECT object->>'object' FROM activities \
         WHERE activity_id = $1 AND local AND type = 'Follow' AND actor_id = $2",
    )
    .bind(&id)
    .bind(actor_id)
    .fetch_optional(&pctx.db)
    .await?;
    if let Some(Some(target_actor)) = follow_target {
        let target_id: Option<i64> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
                .bind(&target_actor)
                .fetch_optional(&pctx.db)
                .await?;
        if let Some(target_id) = target_id {
            follows::unfollow(state, auth.account_id, target_id).await?;
            return Ok(id);
        }
    }

    // Local-target follows publish nothing — their synthetic id is
    // `{actor}#follows/{target_account_id}`.
    if let Some(rest) = id.strip_prefix(&format!("{actor_id}#follows/")) {
        if let Ok(target_id) = rest.parse::<i64>() {
            follows::unfollow(state, auth.account_id, target_id).await?;
            return Ok(id);
        }
    }

    Err(Error::NotFound(format!("activity {id}")))
}
