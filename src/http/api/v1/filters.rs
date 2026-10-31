//! `/api/v1/filters` — the deprecated Mastodon v1 filter API, kept
//! as a compat shim over the epic-20 v2 storage (`filters` +
//! `filter_keywords`). Mobile apps (Moshidon, the official app)
//! still poll it until they see server-side `filtered` results.
//!
//! A v1 filter is one phrase + context; v2 is a titled filter with
//! many keywords. The mapping is one v1 entity per `filter_keywords`
//! row — `id` is the keyword id, `phrase` the keyword, and the
//! parent filter supplies context/expiry/action. Creating through
//! v1 mints a v2 filter titled after the phrase (like Mastodon's
//! own bridge did).

use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::header,
    Json,
};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{
    filters::model,
    http::{middleware::auth::AuthContext, AppState},
    Error, Result,
};

/// v1 wire shape of one (filter, keyword) pair.
fn entity(f: &model::FilterRow, k: &model::KeywordRow) -> Value {
    json!({
        "id": k.id.to_string(),
        "phrase": k.keyword,
        "context": f.context,
        "expires_at": f.expires_at.map(|e| e.to_rfc3339()),
        "irreversible": f.action == "hide",
        "whole_word": k.whole_word,
    })
}

/// All keyword rows owned by the caller, joined to their filters.
async fn keyword_filters(
    db: &PgPool,
    owner: i64,
) -> Result<Vec<(model::FilterRow, model::KeywordRow)>> {
    let filters = model::list(db, owner).await?;
    let mut out = Vec::new();
    for f in filters {
        for k in model::keywords(db, owner, f.id).await? {
            out.push((
                model::FilterRow {
                    id: f.id,
                    title: f.title.clone(),
                    context: f.context.clone(),
                    action: f.action.clone(),
                    expires_at: f.expires_at,
                },
                k,
            ));
        }
    }
    Ok(out)
}

/// `GET /api/v1/filters` — one v1 entity per stored keyword.
#[utoipa::path(
    get,
    path = "/api/v1/filters",
    tag = "filters",
    responses(
        (status = 200, description = "Array of legacy v1 Filter entities"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "read:filters")?;
    let pairs = keyword_filters(&state.db, auth.account_id).await?;
    Ok(Json(json!(pairs
        .iter()
        .map(|(f, k)| entity(f, k))
        .collect::<Vec<_>>())))
}

/// One owned keyword row + its filter, or 404.
async fn get_owned(
    db: &PgPool,
    owner: i64,
    id: i64,
) -> Result<(model::FilterRow, model::KeywordRow)> {
    let kw = model::get_keyword(db, owner, id).await?;
    let f = model::get(db, owner, kw.filter_id).await?;
    Ok((f, kw))
}

/// `GET /api/v1/filters/{id}` — id is a `filter_keywords.id`.
#[utoipa::path(
    get,
    path = "/api/v1/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter (keyword) id")),
    responses(
        (status = 200, description = "The legacy v1 Filter entity"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "read:filters")?;
    let (f, k) = get_owned(&state.db, auth.account_id, id).await?;
    Ok(Json(entity(&f, &k)))
}

/// Parsed v1 create/update body (form or JSON).
#[derive(Default)]
struct V1Params {
    phrase: Option<String>,
    context: Option<Vec<String>>,
    expires_in: Option<i64>,
    irreversible: Option<bool>,
    whole_word: Option<bool>,
}

async fn parse_body(request: Request) -> Result<V1Params> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    let mut out = V1Params::default();
    if content_type.starts_with("application/x-www-form-urlencoded") {
        for (k, v) in url::form_urlencoded::parse(&bytes) {
            match k.as_ref() {
                "phrase" => out.phrase = Some(v.into_owned()),
                "context[]" | "context" => {
                    out.context
                        .get_or_insert_with(Vec::new)
                        .push(v.into_owned());
                }
                "expires_in" => {
                    out.expires_in =
                        Some(v.parse().map_err(|_| {
                            Error::Validation("expires_in must be an integer".into())
                        })?);
                }
                "irreversible" => {
                    out.irreversible = Some(crate::users::settings::parse_bool_str(&v)?);
                }
                "whole_word" => {
                    out.whole_word = Some(crate::users::settings::parse_bool_str(&v)?);
                }
                _ => {}
            }
        }
    } else if !bytes.is_empty() {
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::BadRequest(format!("malformed JSON body: {e}")))?;
        out.phrase = body
            .get("phrase")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.context = body.get("context").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
        out.expires_in = body.get("expires_in").and_then(|v| v.as_i64());
        out.irreversible = body.get("irreversible").and_then(|v| v.as_bool());
        out.whole_word = body.get("whole_word").and_then(|v| v.as_bool());
    }
    Ok(out)
}

/// `POST /api/v1/filters` — mint a v2 filter titled after the phrase
/// with a single keyword, then answer in v1 shape.
#[utoipa::path(
    post,
    path = "/api/v1/filters",
    tag = "filters",
    responses(
        (status = 200, description = "The created v1 Filter entity"),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "write:filters")?;
    let params = parse_body(request).await?;
    let phrase = params
        .phrase
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| Error::Validation("phrase is required".into()))?;
    let context = params.context.unwrap_or_else(|| vec!["home".into()]);
    let action = if params.irreversible.unwrap_or(false) {
        "hide"
    } else {
        "warn"
    };
    let whole_word = params.whole_word.unwrap_or(false);
    let filter = model::create(
        &state.db,
        auth.account_id,
        &phrase,
        &context,
        action,
        params.expires_in,
        &[(phrase.clone(), whole_word)],
    )
    .await?;
    let keywords = model::keywords(&state.db, auth.account_id, filter.id).await?;
    Ok(Json(entity(&filter, &keywords[0])))
}

/// `PUT /api/v1/filters/{id}` — update the keyword's phrase/word flag
/// and/or the parent filter's context/expiry/action.
#[utoipa::path(
    put,
    path = "/api/v1/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter (keyword) id")),
    responses(
        (status = 200, description = "The updated v1 Filter entity"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "write:filters")?;
    let params = parse_body(request).await?;
    let (filter, keyword) = get_owned(&state.db, auth.account_id, id).await?;
    let action = params.irreversible.map(|i| if i { "hide" } else { "warn" });
    let filter = model::update(
        &state.db,
        auth.account_id,
        filter.id,
        None,
        params.context.as_deref(),
        action,
        params.expires_in,
    )
    .await?;
    let keyword = if params.phrase.is_some() || params.whole_word.is_some() {
        model::update_keyword(
            &state.db,
            auth.account_id,
            keyword.id,
            params.phrase.as_deref(),
            params.whole_word,
        )
        .await?
    } else {
        keyword
    };
    Ok(Json(entity(&filter, &keyword)))
}

/// `DELETE /api/v1/filters/{id}` — drop the keyword; when it was the
/// filter's last one the parent goes too.
#[utoipa::path(
    delete,
    path = "/api/v1/filters/{id}",
    tag = "filters",
    params(("id" = String, Path, description = "Filter (keyword) id")),
    responses(
        (status = 200, description = "Deleted"),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_filter(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    crate::auth::scopes::require_scope(&state.config(), &auth, "write:filters")?;
    let (filter, _) = get_owned(&state.db, auth.account_id, id).await?;
    model::delete_keyword(&state.db, auth.account_id, id).await?;
    if model::keywords(&state.db, auth.account_id, filter.id)
        .await?
        .is_empty()
    {
        model::delete(&state.db, auth.account_id, filter.id).await?;
    }
    Ok(Json(json!({})))
}
