//! `/api/v1` relationship endpoints — follow/unfollow, the deprecated
//! `POST /api/v1/follows`, the batch `relationships` read, follower/
//! following listings, user notes and the Yonder `subscribe` extension
//! (epic 14).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, Uri},
    Json,
};
use serde_json::{json, Value};

use crate::{
    activitypub::federation_base_url,
    auth::scopes::require_scope,
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        paginate, AppState,
    },
    social::{self, entity, follows, notes, subscribe},
    users::account as account_mapper,
    Error, Result,
};

// ---------------------------------------------------------------------------
// Body + query param helpers (shared by the social handler files)
// ---------------------------------------------------------------------------

/// Flatten the request body into multi-value params — urlencoded
/// (`reblogs=true&account_ids[]=1&account_ids[]=2`) and JSON
/// (`{"account_ids": [1,2]}`) both land here. Trailing `[]` on keys is
/// stripped so both transports produce the canonical key.
pub(super) async fn body_params(request: Request) -> Result<HashMap<String, Vec<String>>> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    if bytes.is_empty() {
        return Ok(out);
    }
    if content_type.starts_with("application/x-www-form-urlencoded") {
        for (k, v) in url::form_urlencoded::parse(&bytes) {
            let key = k.strip_suffix("[]").unwrap_or(&k).to_string();
            out.entry(key).or_default().push(v.into_owned());
        }
    } else {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Validation(format!("malformed JSON body: {e}")))?;
        if let Value::Object(map) = body {
            for (k, v) in map {
                let key = k.strip_suffix("[]").unwrap_or(&k).to_string();
                match v {
                    Value::Array(items) => {
                        for item in items {
                            if let Some(s) = scalar(&item) {
                                out.entry(key.clone()).or_default().push(s);
                            }
                        }
                    }
                    v => {
                        if let Some(s) = scalar(&v) {
                            out.entry(key).or_default().push(s);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn scalar(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::to_string)
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_bool().map(|b| b.to_string()))
        .or_else(|| v.as_f64().map(|n| n.to_string()))
}

/// First value of `key` (form/JSON normalized by [`body_params`]).
pub(super) fn param<'a>(params: &'a HashMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    params.get(key)?.first().map(String::as_str)
}

/// `key` as a bool (`"true"`/`"1"`), `None` when absent.
pub(super) fn bool_param(params: &HashMap<String, Vec<String>>, key: &str) -> Option<bool> {
    param(params, key).map(|s| s == "true" || s == "1")
}

/// `key` as an i64 vec (handles `account_ids[]` arrays) — 422 on
/// unparseable values rather than silently dropping them.
pub(super) fn id_list(params: &HashMap<String, Vec<String>>, key: &str) -> Result<Vec<i64>> {
    let Some(vs) = params.get(key) else {
        return Ok(Vec::new());
    };
    vs.iter()
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| Error::Validation(format!("invalid {key} value: {s}")))
        })
        .collect()
}

/// `id`/`id[]` values out of a raw query string (`?id[]=1&id=2`).
fn query_ids(uri: &Uri) -> Vec<i64> {
    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "id[]" || k == "id")
        .filter_map(|(_, v)| v.parse().ok())
        .collect()
}

/// Account entities for a list of ids (order preserved) — shared by
/// the social listing handlers (`blocks`, `follow_requests`,
/// `lists`). One emoji-map load serves the whole page via
/// [`account_mapper::account_entities`].
pub(super) async fn account_entities(state: &AppState, ids: &[i64]) -> Result<Vec<Value>> {
    let base = federation_base_url(&state.config);
    let accounts = account_mapper::load_many(&state.db, ids).await?;
    account_mapper::account_entities(&state.db, &state.config, &base, &accounts).await
}

// ---------------------------------------------------------------------------
// Follow / unfollow
// ---------------------------------------------------------------------------

/// `POST /api/v1/accounts/{id}/follow` — `{reblogs?, notify?}`.
/// Returns the Relationship entity.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/follow",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
        (status = 422, description = "The follow is not allowed", body = crate::error::ErrorBody),
    )
)]
pub async fn follow(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    let params = body_params(request).await?;
    let opts = social::FollowOpts {
        show_reblogs: bool_param(&params, "reblogs"),
        notify: bool_param(&params, "notify"),
    };
    follows::visible_target(&state.db, id).await?;
    follows::follow(&state, auth.account_id, id, opts).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/accounts/{id}/unfollow` (and `DELETE …/follow`).
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/unfollow",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn unfollow(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    follows::unfollow(&state, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/follows` (deprecated) — `uri` param (`user@domain`,
/// bare local username or actor URL). Returns the Account entity.
#[utoipa::path(
    post,
    path = "/api/v1/follows",
    tag = "accounts",
    responses(
        (status = 200, description = "The followed account"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn follow_by_uri(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    let params = body_params(request).await?;
    let uri = param(&params, "uri").ok_or_else(|| Error::Validation("uri is required".into()))?;
    let target_id = follows::follow_by_uri(&state, auth.account_id, uri).await?;
    let accounts = account_entities(&state, &[target_id]).await?;
    Ok(Json(accounts.into_iter().next().unwrap_or(json!({}))))
}

/// `GET /api/v1/accounts/relationships?id[]=…` — batch Relationship
/// entities for the caller.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/relationships",
    tag = "accounts",
    responses(
        (status = 200, description = "One Relationship per requested id"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn relationships(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:follows")?;
    let ids = query_ids(&uri);
    Ok(Json(json!(
        entity::relationships(
            &state.db,
            auth.account_id,
            &ids,
            state.config.admin.blockers_visible
        )
        .await?
    )))
}

/// `GET /api/v1/accounts/{id}/followers` — public; empty when the
/// account hides its network (`hide_collections`, stored as `hide_network`).
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{id}/followers",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "Follower accounts (Link-header paginated)"),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn followers(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
    uri: Uri,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config, a, "read:follows")?;
    }
    follows::visible_target(&state.db, id).await?;
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let page = paginate::parse(&params);
    let rows = if follows::network_hidden(&state.db, id, viewer).await? {
        Vec::new()
    } else {
        follows::followers(&state.db, id, &page).await?
    };
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `GET /api/v1/accounts/{id}/following` — public; same
/// `hide_collections` (`hide_network`) semantics as `followers`.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{id}/following",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "Followed accounts (Link-header paginated)"),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn following(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
    uri: Uri,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config, a, "read:follows")?;
    }
    follows::visible_target(&state.db, id).await?;
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let page = paginate::parse(&params);
    let rows = if follows::network_hidden(&state.db, id, viewer).await? {
        Vec::new()
    } else {
        follows::following(&state.db, id, &page).await?
    };
    let cursors: Vec<i64> = rows.iter().map(|(_, c)| *c).collect();
    let ids: Vec<i64> = rows.into_iter().map(|(a, _)| a).collect();
    let accounts = account_entities(&state, &ids).await?;
    let headers = paginate::headers(&uri, &params, &cursors, page.limit);
    Ok((headers, Json(json!(accounts))))
}

/// `POST /api/v1/accounts/{id}/note` — `{comment}` (≤ 2000 chars, `""`
/// clears). Private to the author; returns the Relationship entity.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/note",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
        (status = 422, description = "Note too long", body = crate::error::ErrorBody),
    )
)]
pub async fn note(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:accounts")?;
    let params = body_params(request).await?;
    let comment = param(&params, "comment").unwrap_or_default();
    follows::visible_target(&state.db, id).await?;
    notes::set_note(&state.db, auth.account_id, id, comment).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `POST /api/v1/accounts/{id}/subscribe` — Yonder extension:
/// notify-on-post without following.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/{id}/subscribe",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Account not found", body = crate::error::ErrorBody),
    )
)]
pub async fn subscribe_account(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    follows::visible_target(&state.db, id).await?;
    subscribe::subscribe(&state.db, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}

/// `DELETE /api/v1/accounts/{id}/subscribe` (and `POST …/unsubscribe`).
#[utoipa::path(
    delete,
    path = "/api/v1/accounts/{id}/subscribe",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The relationship"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn unsubscribe_account(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:follows")?;
    subscribe::unsubscribe(&state.db, auth.account_id, id).await?;
    Ok(Json(
        entity::relationship(
            &state.db,
            auth.account_id,
            id,
            state.config.admin.blockers_visible,
        )
        .await?,
    ))
}
