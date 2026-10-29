//! `/api/v1/statuses` — create, read, edit, delete (epic 12).
//! Handlers stay thin: JSON and form bodies normalize into
//! [`StatusParams`], then the [`posts::create`] service runs the
//! pipeline.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, Query, Request, State},
    http::header,
    response::Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    auth::scopes::require_scope,
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        AppState,
    },
    posts::{
        self,
        create::{CreateOutcome, StatusParams},
        entity,
        polls::PollParams,
        PostContext,
    },
    translation::{self, Item, ItemFormat},
    Error, Result,
};

/// `POST /api/v1/statuses` — create. `Idempotency-Key` replays the
/// stored result. Returns the Status (or ScheduledStatus) entity.
#[utoipa::path(
    post,
    path = "/api/v1/statuses",
    tag = "statuses",
    request_body(content_type = "application/json", content = StatusCreateSchema),
    responses(
        (status = 200, description = "The created (or scheduled) status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 409, description = "Idempotent request in flight", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let params = parse_body(request).await?;
    let ctx = PostContext::new(&state);
    match posts::create::create_status(&ctx, &auth, &params, key.as_deref()).await? {
        CreateOutcome::Published(entity) | CreateOutcome::Scheduled(entity) => Ok(Json(entity)),
    }
}

/// `GET /api/v1/statuses/{id}` — the Status entity, subject to
/// `can_see` (anonymous sees public/unlisted only).
#[utoipa::path(
    get,
    path = "/api/v1/statuses/{id}",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status"),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let ctx = PostContext::new(&state);
    let status = posts::load(&ctx.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    if !posts::can_see(&ctx.db, &status, viewer).await? {
        return Err(Error::NotFound("status".into()));
    }
    // A still-pending remote quote self-heals on read — rows ingested
    // before `RemoteStatusFetch` existed get their resolve job here.
    if let (Some(ap), Some(uri)) = (&ctx.ap, &status.quote_uri) {
        if let Err(e) = posts::remote::enqueue_quote_resolve(&ap.core, uri).await {
            tracing::warn!(status_id = status.id, error = %e, "quote resolve enqueue failed");
        }
    }
    // A remote A/V object with no media self-heals on read — rows
    // ingested before `url`-Link synthesis existed (PeerTube `Video`s
    // carry no `attachment`) get a refresh job that lands the playable
    // attachment + poster. `object_type` NULL rows (pre-migration) get
    // a single refresh — the re-ingest writes the type, so a legacy
    // media-less `Note` only fetches once.
    if let (Some(ap), Some(uri)) = (&ctx.ap, &status.uri) {
        let maybe_av = matches!(
            status.object_type.as_deref(),
            None | Some("Video") | Some("Audio")
        );
        if !status.local && maybe_av {
            let has_media: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM media_attachments WHERE status_id = $1)",
            )
            .bind(status.id)
            .fetch_one(&ctx.db)
            .await?;
            if !has_media {
                if let Err(e) = posts::remote::enqueue_media_refresh(&ap.core, uri).await {
                    tracing::warn!(status_id = status.id, error = %e, "media refresh enqueue failed");
                }
            }
        }
    }
    Ok(Json(entity::status_entity(&ctx, &status, viewer).await?))
}

/// `GET /api/v1/statuses/{id}/context` — the thread context: visible ancestors
/// (root first) and descendants (flat, chronological). The status
/// itself must be `can_see`-visible to the viewer — otherwise the
/// whole context 404s, never leaking existence.
#[utoipa::path(
    get,
    path = "/api/v1/statuses/{id}/context",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The status's ancestors and descendants"),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
    )
)]
pub async fn context(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    let ctx = PostContext::new(&state);
    let status = posts::load(&ctx.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    if !posts::can_see(&ctx.db, &status, viewer).await? {
        return Err(Error::NotFound("status".into()));
    }
    let ancestor_rows = posts::context::ancestors(&ctx.db, id, viewer).await?;
    let descendant_rows = posts::context::descendants(&ctx.db, id, viewer).await?;
    // Ancestors and descendants share one render batch — the whole
    // thread is a single wave of lookups, not two.
    let ancestor_len = ancestor_rows.len();
    let mut all = ancestor_rows;
    all.extend(descendant_rows);
    let mut rendered = entity::status_entities(&ctx, &all, viewer).await?;
    let descendants = rendered.split_off(ancestor_len);
    let ancestors = rendered;
    Ok(Json(json!({
        "ancestors": ancestors,
        "descendants": descendants,
    })))
}

/// `PUT /api/v1/statuses/{id}` — owner edit; republishes `Update`.
#[utoipa::path(
    put,
    path = "/api/v1/statuses/{id}",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    request_body(content_type = "application/json", content = StatusCreateSchema),
    responses(
        (status = 200, description = "The updated status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
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
    require_scope(&state.config(), &auth, "write:statuses")?;
    let params = parse_body(request).await?;
    let ctx = PostContext::new(&state);
    let status = posts::create::edit_status(&ctx, &auth, id, &params).await?;
    Ok(Json(
        entity::status_entity(&ctx, &status, Some(auth.account_id)).await?,
    ))
}

/// `DELETE /api/v1/statuses/{id}` — owner delete; tombstones and
/// publishes `Delete`. Returns the pre-delete entity.
#[utoipa::path(
    delete,
    path = "/api/v1/statuses/{id}",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    responses(
        (status = 200, description = "The deleted status"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "Not found", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_status(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:statuses")?;
    let ctx = PostContext::new(&state);
    let before = posts::load(&ctx.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    let entity = entity::status_entity(&ctx, &before, Some(auth.account_id)).await?;
    posts::create::delete_status(&ctx, auth.account_id, id).await?;
    Ok(Json(entity))
}

/// `POST /api/v1/statuses/{id}/translate` — the Mastodon-compatible
/// Translation entity. `can_see` is the only audience gate: the admin
/// chose the backend (which may be a self-hosted LibreTranslate), so
/// gated content is sent to it just like Mastodon sends public posts.
/// The target language is `lang` (query or body) → the viewer's
/// interface locale → `i18n.fallback_locale`.
#[utoipa::path(
    post,
    path = "/api/v1/statuses/{id}/translate",
    tag = "statuses",
    params(("id" = String, Path, description = "Status id")),
    request_body(content_type = "application/json", content = TranslateSchema),
    responses(
        (status = 200, description = "The translation"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 404, description = "Not found or not visible", body = crate::error::ErrorBody),
        (status = 422, description = "Not configured or bad lang", body = crate::error::ErrorBody),
    )
)]
pub async fn translate(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(query): Query<TranslateQuery>,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let config = state.config().clone();
    if !translation::enabled(&config.content) {
        return Err(Error::Validation(
            "translation is not configured on this server".into(),
        ));
    }
    let ctx = PostContext::new(&state);
    let status = posts::load(&ctx.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if !posts::can_see(&ctx.db, &status, Some(auth.account_id)).await? {
        return Err(Error::NotFound("status".into()));
    }
    let body_lang = translate_body_lang(request).await?;
    let target = match query.lang.or(body_lang) {
        Some(lang) => {
            // Loose BCP-47 — `en`, `pt-BR`, `zh-Hant`; anything else
            // is a client bug, not a backend request.
            if lang.len() > 35
                || !lang
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(Error::Validation("invalid lang".into()));
            }
            lang
        }
        None => crate::users::settings::load(&state.db, auth.user_id)
            .await?
            .locale
            .unwrap_or_else(|| config.i18n.fallback_locale.clone()),
    };

    // The same HTML the Status entity renders — translating it keeps
    // mention/hashtag links intact through tag-aware backends.
    let content = status
        .content_html
        .clone()
        .unwrap_or_else(|| pubby_sdk::content::html_escape(&status.content));
    let spoiler = status
        .spoiler_text
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let poll_options: Vec<String> = match status.poll_id {
        Some(pid) => {
            sqlx::query_scalar::<_, String>(
                "SELECT title FROM poll_options WHERE poll_id = $1 ORDER BY idx",
            )
            .bind(pid)
            .fetch_all(&ctx.db)
            .await?
        }
        None => Vec::new(),
    };
    let media: Vec<(i64, String)> = sqlx::query_as::<_, (i64, String)>(
        "SELECT id, description FROM media_attachments \
         WHERE status_id = $1 AND description <> '' ORDER BY id",
    )
    .bind(id)
    .fetch_all(&ctx.db)
    .await?;

    // One Item per string, in assembly order: content, spoiler, poll
    // options, media descriptions.
    let mut items = vec![Item {
        text: content,
        format: ItemFormat::Html,
    }];
    if let Some(s) = &spoiler {
        items.push(Item {
            text: s.clone(),
            format: ItemFormat::Plain,
        });
    }
    items.extend(poll_options.iter().cloned().map(|text| Item {
        text,
        format: ItemFormat::Plain,
    }));
    items.extend(media.iter().map(|(_, d)| Item {
        text: d.clone(),
        format: ItemFormat::Plain,
    }));

    // Edits (body, CW) and alt-text updates change the source — the
    // fingerprint invalidates stale entries without a schema version.
    let fingerprint = {
        let mut h = Sha256::new();
        for item in &items {
            h.update(item.text.as_bytes());
            h.update([0]);
        }
        hex::encode(h.finalize())
    };
    let cache_key = format!("translate:{}:{}:{}", id, target, &fingerprint[..16]);
    const TRANSLATE_TTL: Duration = Duration::from_secs(24 * 3600);
    let poll_n = poll_options.len();
    let translated = state
        .cache
        .get_or_load(&cache_key, TRANSLATE_TTL, || async {
            translation::translate_all(&config, &items, &target)
                .await
                .map(|o| (o.texts, o.detected_language, o.provider.to_string()))
        })
        .await?;

    let mut texts = translated.0.into_iter();
    let content_t = texts.next().unwrap_or_default();
    let spoiler_t = spoiler.map(|_| texts.next().unwrap_or_default());
    let poll_t: Vec<String> = (0..poll_n)
        .map(|_| texts.next().unwrap_or_default())
        .collect();
    let media_t: Vec<String> = texts.collect();
    debug_assert_eq!(poll_t.len(), poll_n);
    debug_assert_eq!(media_t.len(), media.len());

    Ok(Json(json!({
        "content": content_t,
        "detected_source_language": translated.1,
        "media_attachments": media
            .iter()
            .zip(media_t)
            .map(|((mid, _), description)| json!({
                "id": mid.to_string(),
                "description": description,
            }))
            .collect::<Vec<_>>(),
        "poll": match status.poll_id {
            Some(pid) => json!({
                "id": pid.to_string(),
                "options": poll_t.into_iter().map(|title| json!({"title": title})).collect::<Vec<_>>(),
            }),
            None => Value::Null,
        },
        "provider": translated.2.as_str(),
        "spoiler_text": spoiler_t.unwrap_or_default(),
    })))
}

/// `lang` may arrive as a query param or inside the body (JSON or
/// form — Mastodon clients differ).
#[derive(Debug, serde::Deserialize)]
pub struct TranslateQuery {
    pub lang: Option<String>,
}

async fn translate_body_lang(request: Request) -> Result<Option<String>> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if content_type.starts_with("application/x-www-form-urlencoded") {
        Ok(url::form_urlencoded::parse(&bytes)
            .find(|(k, _)| k == "lang")
            .map(|(_, v)| v.into_owned()))
    } else {
        let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        Ok(body
            .get("lang")
            .and_then(|v| v.as_str())
            .map(str::to_string))
    }
}

/// OpenAPI shape of the translate body.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TranslateSchema {
    /// BCP-47 target language.
    pub lang: Option<String>,
}

// ---------------------------------------------------------------------------
// Body parsing — JSON and application/x-www-form-urlencoded (Mastodon
// clients send both; `media_ids[]`/`poll[…]` are the form shapes).
// ---------------------------------------------------------------------------

/// OpenAPI shape of the JSON create/update body.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StatusCreateSchema {
    /// Raw source text.
    pub status: Option<String>,
    /// `text/plain` / `text/markdown` (yonder extension).
    pub content_type: Option<String>,
    /// `public|unlisted|private|direct`.
    pub visibility: Option<String>,
    /// BCP-47 language.
    pub language: Option<String>,
    /// Mark sensitive.
    pub sensitive: Option<bool>,
    /// Content warning.
    pub spoiler_text: Option<String>,
    /// Reply parent id.
    pub in_reply_to_id: Option<String>,
    /// Attachment ids.
    pub media_ids: Option<Vec<String>>,
    /// Poll definition.
    pub poll: Option<PollSchema>,
    /// Quoted status id (epic 13).
    pub quote_id: Option<String>,
    /// Future RFC-3339 time → scheduled post.
    pub scheduled_at: Option<String>,
}

/// `poll` object shape.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PollSchema {
    pub options: Vec<String>,
    pub expires_in: u64,
    pub multiple: Option<bool>,
    pub hide_totals: Option<bool>,
}

fn opt_bool(v: Option<&Value>, key: &str) -> Result<Option<bool>> {
    match v {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) => Ok(Some(crate::users::settings::parse_bool_str(s)?)),
        _ => Err(Error::Validation(format!("{key} must be a boolean"))),
    }
}

fn opt_i64(v: Option<&Value>, key: &str) -> Result<Option<i64>> {
    match v {
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

fn opt_time(v: Option<&Value>, key: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| Some(t.with_timezone(&chrono::Utc)))
            .map_err(|_| Error::Validation(format!("{key} must be an RFC-3339 timestamp"))),
        _ => Err(Error::Validation(format!("{key} must be a timestamp"))),
    }
}

fn poll_from_json(v: Option<&Value>) -> Result<Option<PollParams>> {
    let Some(v) = v else { return Ok(None) };
    let options: Vec<String> = v
        .get("options")
        .and_then(|o| o.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|o| o.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let expires_in = opt_i64(v.get("expires_in"), "poll.expires_in")?.unwrap_or(0) as u64;
    Ok(Some(PollParams {
        options,
        expires_in,
        multiple: opt_bool(v.get("multiple"), "poll.multiple")?.unwrap_or(false),
        hide_totals: opt_bool(v.get("hide_totals"), "poll.hide_totals")?.unwrap_or(false),
    }))
}

fn params_from_json(body: &Value) -> Result<StatusParams> {
    Ok(StatusParams {
        status: body
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        content_type: body
            .get("content_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        visibility: body
            .get("visibility")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        language: body
            .get("language")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        sensitive: opt_bool(body.get("sensitive"), "sensitive")?,
        spoiler_text: body
            .get("spoiler_text")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        in_reply_to_id: opt_i64(body.get("in_reply_to_id"), "in_reply_to_id")?,
        media_ids: body.get("media_ids").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|i| {
                    i.as_str()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| i.as_i64())
                })
                .collect()
        }),
        poll: poll_from_json(body.get("poll"))?,
        quote_id: opt_i64(body.get("quote_id"), "quote_id")?,
        scheduled_at: opt_time(body.get("scheduled_at"), "scheduled_at")?,
    })
}

/// Form bodies use `media_ids[]` (repeated) and `poll[options][]`,
/// `poll[expires_in]`, `poll[multiple]`, `poll[hide_totals]`.
fn params_from_pairs(pairs: &[(String, String)]) -> Result<StatusParams> {
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let repeated = |k: &str| {
        pairs
            .iter()
            .filter(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    let mut poll = None;
    let poll_options = repeated("poll[options][]");
    if !poll_options.is_empty() {
        poll = Some(PollParams {
            options: poll_options,
            expires_in: get("poll[expires_in]")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            multiple: get("poll[multiple]")
                .map(crate::users::settings::parse_bool_str)
                .transpose()?
                .unwrap_or(false),
            hide_totals: get("poll[hide_totals]")
                .map(crate::users::settings::parse_bool_str)
                .transpose()?
                .unwrap_or(false),
        });
    }
    Ok(StatusParams {
        status: get("status").unwrap_or_default().to_string(),
        content_type: get("content_type").map(str::to_string),
        visibility: get("visibility").map(str::to_string),
        language: get("language").map(str::to_string),
        sensitive: get("sensitive")
            .map(crate::users::settings::parse_bool_str)
            .transpose()?,
        spoiler_text: get("spoiler_text").map(str::to_string),
        in_reply_to_id: get("in_reply_to_id")
            .map(|s| {
                s.parse()
                    .map_err(|_| Error::Validation("in_reply_to_id must be an integer".into()))
            })
            .transpose()?,
        media_ids: {
            let raw = repeated("media_ids[]");
            if raw.is_empty() {
                None
            } else {
                Some(raw.iter().filter_map(|s| s.parse().ok()).collect())
            }
        },
        poll,
        quote_id: get("quote_id")
            .map(|s| {
                s.parse()
                    .map_err(|_| Error::Validation("quote_id must be an integer".into()))
            })
            .transpose()?,
        scheduled_at: match get("scheduled_at") {
            Some(s) => Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .map_err(|_| {
                        Error::Validation("scheduled_at must be an RFC-3339 timestamp".into())
                    })?,
            ),
            None => None,
        },
    })
}

/// Normalize the request body per its content type.
async fn parse_body(request: Request) -> Result<StatusParams> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024)
        .await
        .map_err(|e| Error::Validation(format!("failed to read body: {e}")))?;
    if content_type.starts_with("application/x-www-form-urlencoded") {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&bytes)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        params_from_pairs(&pairs)
    } else {
        let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        params_from_json(&body)
    }
}
