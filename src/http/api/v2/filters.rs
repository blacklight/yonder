//! `/api/v2/filters` — server-side content filters (epic 20):
//! filter CRUD plus `keywords` and `statuses` sub-resources, all
//! owner-scoped (a foreign id is a 404).

use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::header,
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

use crate::{
    auth::scopes::require_scope,
    filters::{entity, model},
    http::{middleware::auth::AuthContext, AppState},
    Result,
};

/// Shared tail for filter reads — keywords + pinned ids attached.
async fn with_children(db: &PgPool, owner: i64, row: &model::FilterRow) -> Result<Json<Value>> {
    let keywords = model::keywords(db, owner, row.id).await?;
    let statuses = model::pinned_statuses(db, owner, row.id).await?;
    Ok(Json(entity::filter_entity(row, &keywords, &statuses)))
}

/// `keywords_attributes` entry for create — Mastodon's nested form.
#[derive(Debug, Deserialize)]
pub struct KeywordAttr {
    pub keyword: Option<String>,
    pub whole_word: Option<bool>,
}

/// Filter create/update body (JSON or form-decoded by the caller).
#[derive(Debug, Deserialize)]
pub struct FilterBody {
    pub title: Option<String>,
    pub context: Option<Vec<String>>,
    pub filter_action: Option<String>,
    pub expires_in: Option<i64>,
    pub keywords_attributes: Option<Vec<KeywordAttr>>,
}

// ---------------------------------------------------------------------------
// Body parsing — JSON and application/x-www-form-urlencoded. Mastodon
// clients submit filters as forms: `context[]` repeats, and keywords
// arrive as `keywords_attributes[0][keyword]` /
// `keywords_attributes[0][whole_word]` (the `[index]` may also be
// `[]`, depending on the client).
// ---------------------------------------------------------------------------

/// Read the request body as form pairs or a JSON value, per
/// Content-Type. A non-empty body that isn't valid JSON is a 400 —
/// silently degrading to `{}` made malformed PUTs no-op 200s. An
/// empty body stays `Null` so the handler's own "field required"
/// errors fire.
async fn body_pairs(request: Request) -> Result<(Vec<(String, String)>, Value)> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 256 * 1024)
        .await
        .map_err(|e| crate::Error::Validation(format!("failed to read body: {e}")))?;
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let pairs = url::form_urlencoded::parse(&bytes)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        Ok((pairs, Value::Null))
    } else if bytes.is_empty() {
        Ok((Vec::new(), Value::Null))
    } else {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| crate::Error::BadRequest(format!("malformed JSON body: {e}")))?;
        Ok((Vec::new(), body))
    }
}

/// `keywords_attributes[N][keyword|whole_word]` entries from form
/// pairs, grouped by `N` — an interleaved submission
/// (`[0][keyword]`, `[1][keyword]`, `[0][whole_word]`, …) must not
/// let index 0's flag land on index 1's entry. Numerically ordered;
/// unindexed `[]` fields collect in submission order.
fn keyword_attrs_from_pairs(pairs: &[(String, String)]) -> Vec<KeywordAttr> {
    let mut by_index: std::collections::BTreeMap<(u32, String), KeywordAttr> = Default::default();
    let mut bare: Vec<KeywordAttr> = Vec::new();
    for (k, v) in pairs {
        let Some(rest) = k.strip_prefix("keywords_attributes[") else {
            continue;
        };
        let Some((index, field)) = rest
            .split_once("][")
            .map(|(i, f)| (i.trim_end_matches(']'), f.trim_end_matches(']')))
        else {
            continue;
        };
        if index.is_empty() {
            // `keywords_attributes[][field]` — append in order.
            match field {
                "keyword" => bare.push(KeywordAttr {
                    keyword: Some(v.clone()),
                    whole_word: None,
                }),
                "whole_word" => {
                    let parsed = crate::users::settings::parse_bool_str(v).ok();
                    match bare.last_mut() {
                        Some(kw) => kw.whole_word = parsed.or(kw.whole_word),
                        None => bare.push(KeywordAttr {
                            keyword: None,
                            whole_word: parsed,
                        }),
                    }
                }
                _ => {}
            }
            continue;
        }
        let order = index.parse::<u32>().unwrap_or(u32::MAX);
        let entry = by_index
            .entry((order, index.to_string()))
            .or_insert_with(|| KeywordAttr {
                keyword: None,
                whole_word: None,
            });
        match field {
            "keyword" => entry.keyword = Some(v.clone()),
            "whole_word" => {
                let parsed = crate::users::settings::parse_bool_str(v).ok();
                entry.whole_word = parsed.or(entry.whole_word);
            }
            _ => {}
        }
    }
    by_index.into_values().chain(bare).collect()
}

/// `FilterBody` from either wire shape. Form `context[]`/`context`
/// repeats collect; `expires_in` parses as an integer.
fn filter_body_from(pairs: &[(String, String)], json: &Value) -> Result<FilterBody> {
    if json.is_object() {
        let mut body: FilterBody = serde_json::from_value(json.clone())
            .map_err(|e| crate::Error::Validation(format!("invalid filter body: {e}")))?;
        // Mastodon's JSON wire is `context[]` too — accept it.
        if body.context.is_none() {
            body.context = json
                .get("context[]")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
        }
        return Ok(body);
    }
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let context: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "context[]" || k == "context")
        .map(|(_, v)| v.clone())
        .collect();
    Ok(FilterBody {
        title: get("title"),
        context: if context.is_empty() {
            None
        } else {
            Some(context)
        },
        filter_action: get("filter_action"),
        expires_in: get("expires_in")
            .map(|s| {
                s.parse::<i64>()
                    .map_err(|_| crate::Error::Validation("expires_in must be an integer".into()))
            })
            .transpose()?,
        keywords_attributes: {
            let attrs = keyword_attrs_from_pairs(pairs);
            if attrs.is_empty() {
                None
            } else {
                Some(attrs)
            }
        },
    })
}

/// `KeywordAttr` from either wire shape.
fn keyword_attr_from(pairs: &[(String, String)], json: &Value) -> Result<KeywordAttr> {
    if json.is_object() {
        return serde_json::from_value(json.clone())
            .map_err(|e| crate::Error::Validation(format!("invalid keyword body: {e}")));
    }
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    Ok(KeywordAttr {
        keyword: get("keyword"),
        whole_word: get("whole_word")
            .map(|s| crate::users::settings::parse_bool_str(&s))
            .transpose()?,
    })
}

/// A bare `status_id` from either wire shape.
fn status_id_from(pairs: &[(String, String)], json: &Value) -> Result<i64> {
    let raw = if json.is_object() {
        json.get("status_id").and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or(v.as_i64().map(|i| i.to_string()))
        })
    } else {
        pairs
            .iter()
            .find(|(k, _)| k == "status_id")
            .map(|(_, v)| v.clone())
    };
    raw.ok_or_else(|| crate::Error::Validation("status_id is required".into()))?
        .parse()
        .map_err(|_| crate::Error::Validation("status_id must be an integer".into()))
}

/// `GET /api/v2/filters` — the caller's filters, newest-first.
#[utoipa::path(
    get,
    path = "/api/v2/filters",
    tag = "filters",
    responses(
        (status = 200, description = "Array of Filter entities"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:filters")?;
    let filters = model::list(&state.db, auth.account_id).await?;
    let mut out = Vec::with_capacity(filters.len());
    for f in &filters {
        let kws = model::keywords(&state.db, auth.account_id, f.id).await?;
        let ss = model::pinned_statuses(&state.db, auth.account_id, f.id).await?;
        out.push(entity::filter_entity(f, &kws, &ss));
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v2/filters` — create a filter with optional nested
/// `keywords_attributes`.
#[utoipa::path(
    post,
    path = "/api/v2/filters",
    tag = "filters",
    request_body = Value,
    responses(
        (status = 200, description = "The created Filter"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failed", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    let (pairs, json) = body_pairs(request).await?;
    let body = filter_body_from(&pairs, &json)?;
    let keywords: Vec<(String, bool)> = body
        .keywords_attributes
        .unwrap_or_default()
        .iter()
        .map(|k| {
            (
                k.keyword.clone().unwrap_or_default(),
                k.whole_word.unwrap_or(true),
            )
        })
        .collect();
    if keywords.len() as u32 > state.config.content.max_filter_keywords {
        return Err(crate::Error::Validation(format!(
            "at most {} keywords per filter",
            state.config.content.max_filter_keywords
        )));
    }
    // Soft cap: check-then-insert — two racing creates can each
    // land one filter over the limit. Acceptable for a courtesy
    // bound (unlike per-keyword counts, nothing depends on it).
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM filters WHERE account_id = $1")
        .bind(auth.account_id)
        .fetch_one(&state.db)
        .await?;
    if count >= state.config.content.max_filters as i64 {
        return Err(crate::Error::Validation(format!(
            "at most {} filters per account",
            state.config.content.max_filters
        )));
    }
    let row = model::create(
        &state.db,
        auth.account_id,
        &body.title.unwrap_or_default(),
        &body.context.unwrap_or_default(),
        body.filter_action.as_deref().unwrap_or("warn"),
        body.expires_in,
        &keywords,
    )
    .await?;
    with_children(&state.db, auth.account_id, &row).await
}

/// `GET /api/v2/filters/{id}` — a single owned filter.
#[utoipa::path(
    get,
    path = "/api/v2/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    responses(
        (status = 200, description = "The Filter"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:filters")?;
    let row = model::get(&state.db, auth.account_id, id).await?;
    with_children(&state.db, auth.account_id, &row).await
}

/// `PUT /api/v2/filters/{id}` — partial update.
#[utoipa::path(
    put,
    path = "/api/v2/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    request_body = Value,
    responses(
        (status = 200, description = "The updated Filter"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    let (pairs, json) = body_pairs(request).await?;
    let body = filter_body_from(&pairs, &json)?;
    let row = model::update(
        &state.db,
        auth.account_id,
        id,
        body.title.as_deref(),
        body.context.as_deref(),
        body.filter_action.as_deref(),
        body.expires_in,
    )
    .await?;
    with_children(&state.db, auth.account_id, &row).await
}

/// `DELETE /api/v2/filters/{id}` — empty response body.
#[utoipa::path(
    delete,
    path = "/api/v2/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    responses(
        (status = 200, description = "Deleted (empty body)"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_filter(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    model::delete(&state.db, auth.account_id, id).await?;
    Ok(Json(Value::Object(Default::default())))
}

/// `GET /api/v2/filters/{id}/keywords` — a filter's keywords.
#[utoipa::path(
    get,
    path = "/api/v2/filters/{id}/keywords",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    responses(
        (status = 200, description = "Array of FilterKeyword entities"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn keywords(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:filters")?;
    let kws = model::keywords(&state.db, auth.account_id, id).await?;
    Ok(Json(Value::Array(
        kws.iter().map(entity::keyword_entity).collect(),
    )))
}

/// `POST /api/v2/filters/{id}/keywords` — add a keyword.
#[utoipa::path(
    post,
    path = "/api/v2/filters/{id}/keywords",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    request_body = Value,
    responses(
        (status = 200, description = "The new FilterKeyword"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failed", body = crate::error::ErrorBody),
    )
)]
pub async fn add_keyword(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    let (pairs, json) = body_pairs(request).await?;
    let body = keyword_attr_from(&pairs, &json)?;
    let row = model::add_keyword(
        &state.db,
        auth.account_id,
        id,
        &body.keyword.unwrap_or_default(),
        body.whole_word.unwrap_or(true),
        state.config.content.max_filter_keywords,
    )
    .await?;
    Ok(Json(entity::keyword_entity(&row)))
}

/// `GET /api/v2/filters/keywords/{id}` — one keyword.
#[utoipa::path(
    get,
    path = "/api/v2/filters/keywords/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "FilterKeyword id")),
    responses(
        (status = 200, description = "The FilterKeyword"),
        (status = 404, description = "No such keyword", body = crate::error::ErrorBody),
    )
)]
pub async fn show_keyword(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:filters")?;
    let row = model::get_keyword(&state.db, auth.account_id, id).await?;
    Ok(Json(entity::keyword_entity(&row)))
}

/// `PUT /api/v2/filters/keywords/{id}` — update a keyword.
#[utoipa::path(
    put,
    path = "/api/v2/filters/keywords/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "FilterKeyword id")),
    request_body = Value,
    responses(
        (status = 200, description = "The updated FilterKeyword"),
        (status = 404, description = "No such keyword", body = crate::error::ErrorBody),
    )
)]
pub async fn update_keyword(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    let (pairs, json) = body_pairs(request).await?;
    let body = keyword_attr_from(&pairs, &json)?;
    let row = model::update_keyword(
        &state.db,
        auth.account_id,
        id,
        body.keyword.as_deref(),
        body.whole_word,
    )
    .await?;
    Ok(Json(entity::keyword_entity(&row)))
}

/// `DELETE /api/v2/filters/keywords/{id}` — remove a keyword.
#[utoipa::path(
    delete,
    path = "/api/v2/filters/keywords/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "FilterKeyword id")),
    responses(
        (status = 200, description = "Deleted (empty body)"),
        (status = 404, description = "No such keyword", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_keyword(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    model::delete_keyword(&state.db, auth.account_id, id).await?;
    Ok(Json(Value::Object(Default::default())))
}

/// `GET /api/v2/filters/{id}/statuses` — a filter's pinned statuses.
#[utoipa::path(
    get,
    path = "/api/v2/filters/{id}/statuses",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    responses(
        (status = 200, description = "Array of FilterStatus entities"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn statuses(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "read:filters")?;
    let pins = model::pinned_statuses(&state.db, auth.account_id, id).await?;
    Ok(Json(Value::Array(
        pins.iter()
            .map(|(pin, sid)| entity::filter_status_entity(*pin, *sid))
            .collect(),
    )))
}

/// `POST /api/v2/filters/{id}/statuses` — pin a status to a filter.
#[utoipa::path(
    post,
    path = "/api/v2/filters/{id}/statuses",
    tag = "filters",
    params(("id" = String, Path, description = "Filter id")),
    request_body = Value,
    responses(
        (status = 200, description = "The FilterStatus pin"),
        (status = 404, description = "No such filter", body = crate::error::ErrorBody),
    )
)]
pub async fn add_status(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    let (pairs, json) = body_pairs(request).await?;
    let status_id = status_id_from(&pairs, &json)?;
    // The pinned status must exist and be visible to the filter
    // owner — otherwise a pin could anchor to an unseen row.
    let status = crate::posts::load(&state.db, status_id)
        .await?
        .ok_or_else(|| crate::Error::NotFound("status".into()))?;
    if status.tombstone || !crate::posts::can_see(&state.db, &status, Some(auth.account_id)).await?
    {
        return Err(crate::Error::NotFound("status".into()));
    }
    let pin_id = model::pin_status(&state.db, auth.account_id, id, status_id).await?;
    Ok(Json(entity::filter_status_entity(pin_id, status_id)))
}

/// `DELETE /api/v2/filters/statuses/{id}` — remove a status pin.
#[utoipa::path(
    delete,
    path = "/api/v2/filters/statuses/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "FilterStatus pin id")),
    responses(
        (status = 200, description = "Deleted (empty body)"),
        (status = 404, description = "No such pin", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_status(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config, &auth, "write:filters")?;
    model::unpin_status(&state.db, auth.account_id, id).await?;
    Ok(Json(Value::Object(Default::default())))
}
