//! `/api/v1/accounts` — registration (Mastodon `POST /api/v1/accounts`),
//! `verify_credentials`, the profile-page reads (`GET /{id}`,
//! `GET /{id}/statuses` — epic 18), and the `verify_email` token
//! endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{registration, scopes::require_scope, tokens},
    http::{
        middleware::auth::{bearer_token, AuthContext, MaybeAuth},
        paginate, AppState,
    },
    timelines::{Filters, TimelineScope},
    users::{account as account_mapper, profile, settings},
    Error, Result,
};

/// `POST /api/v1/accounts` — register. An optional bearer **app token**
/// (client_credentials) binds the returned user token to that app;
/// anonymous registration is allowed.
#[utoipa::path(
    post,
    path = "/api/v1/accounts",
    tag = "accounts",
    request_body = registration::RegisterParams,
    responses(
        (status = 200, description = "Account created; returns an access token"),
        (status = 401, description = "Invalid app token", body = crate::error::ErrorBody),
        (status = 403, description = "Registrations closed", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Json(params): Json<registration::RegisterParams>,
) -> Result<Json<Value>> {
    // App token (client_credentials, user_id NULL) — a dedicated lookup,
    // not the user-only AuthContext. A presented-but-invalid token 401s;
    // infrastructure errors propagate as 5xx (a DB outage isn't a bad
    // token).
    let app = match bearer_token(&headers, &uri) {
        Some(raw) => Some(
            tokens::resolve_app_token(&state, &raw)
                .await?
                .ok_or(Error::Unauthorized)?,
        ),
        None => None,
    };

    // Account + actor + first token pair commit in one transaction.
    let (registered, issued) = registration::register(&state, &params, app.as_ref()).await?;
    // `verification_pending` rides on the token entity so the SPA can
    // show "check your email" — with `require_email_verification` on,
    // writes stay gated until `verify_email` consumes the token, and
    // without this flag the client has no way to explain the 403s.
    let mut body = tokens::token_entity(&issued);
    body["verification_pending"] = json!(registered.verification_pending);
    Ok(Json(body))
}

/// `GET /api/v1/accounts/verify_credentials` — the caller's Account entity.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/verify_credentials",
    tag = "accounts",
    responses(
        (status = 200, description = "The authenticated account"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn verify_credentials(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    credential_account(
        &state,
        &auth,
        settings::accept_language(&headers).as_deref(),
    )
    .await
}

/// The CredentialAccount entity: `to_account` plus the own-account
/// `source` block and the `yonder.settings` extension — shared by
/// `verify_credentials` and `update_credentials`.
///
/// `source.language`/`source.privacy`/`source.sensitive` are the
/// Mastodon *posting* defaults: `language` resolves stored posting
/// language → stored interface language → `Accept-Language` →
/// `i18n.fallback_locale` (see [`settings::effective_language`]).
async fn credential_account(
    state: &Arc<AppState>,
    auth: &AuthContext,
    accept_language: Option<&str>,
) -> Result<Json<Value>> {
    let account = account_mapper::load(&state.db, auth.account_id)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))?;
    let user_settings = settings::load(&state.db, auth.user_id).await?;
    let base_url = crate::activitypub::federation_base_url(&state.config());
    let mut entity =
        account_mapper::account_entity(&state.db, &state.config(), &base_url, &account).await?;
    entity["source"] = json!({
        "privacy": settings::public_visibility(&user_settings.default_visibility),
        "sensitive": user_settings.default_sensitive,
        "language": settings::effective_language(
            user_settings.default_language.as_deref(),
            user_settings.locale.as_deref(),
            accept_language,
            &state.config(),
        ),
        "note": account.bio.clone().unwrap_or_default(),
        "fields": account_mapper::source_fields(&account.profile_fields),
    });
    // Mastodon positions these on the CredentialAccount top level,
    // not inside `source`.
    entity["hide_collections"] = json!(user_settings.hide_network);
    entity["indexable"] = json!(user_settings.indexable);
    entity["yonder"]["settings"] = user_settings.yonder_json(&state.config(), accept_language);
    // `yonder.role` feeds the SPA's staff-only UI; the top-level
    // `role` keeps Mastodon compatibility (permission bits are not
    // modeled — the object is a name carrier). Mastodon serializes
    // the bitmask as a decimal string, so "0" rather than "".
    entity["yonder"]["role"] = json!(auth.role);
    entity["role"] = json!({
        "id": auth.role,
        "name": auth.role,
        "permissions": "0",
        "color": "",
    });
    Ok(Json(entity))
}

/// `PATCH /api/v1/accounts/update_credentials` — edit the caller's
/// profile (display name, bio, avatar/header, ordered fields, and the
/// `locked`/`bot`/`discoverable` flags). Accepts
/// `multipart/form-data` (file uploads), `application/x-www-form-urlencoded`
/// and `application/json` (no files). Returns the CredentialAccount.
#[utoipa::path(
    patch,
    path = "/api/v1/accounts/update_credentials",
    tag = "accounts",
    request_body(content_type = "application/json", content = crate::users::profile::UpdateCredentialsSchema),
    responses(
        (status = 200, description = "Updated CredentialAccount"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope or account-state gate", body = crate::error::ErrorBody),
        (status = 413, description = "Body exceeds the upload limit", body = crate::error::ErrorBody),
        (status = 422, description = "Validation failure", body = crate::error::ErrorBody),
    )
)]
pub async fn update_credentials(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    request: Request,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "write:accounts")?;
    let accept_language = settings::accept_language(request.headers());
    let params = profile::parse_request(&state, request).await?;
    profile::update_profile(&state, &auth, params).await?;
    credential_account(&state, &auth, accept_language.as_deref()).await
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct VerifyEmailParams {
    pub token: String,
}

/// `POST /api/v1/accounts/verify_email` — consume an emailed token.
#[utoipa::path(
    post,
    path = "/api/v1/accounts/verify_email",
    tag = "accounts",
    request_body = VerifyEmailParams,
    responses(
        (status = 200, description = "Email verified"),
        (status = 422, description = "Invalid or expired token", body = crate::error::ErrorBody),
    )
)]
pub async fn verify_email(
    State(state): State<Arc<AppState>>,
    Json(params): Json<VerifyEmailParams>,
) -> Result<Json<Value>> {
    if !registration::verify_email(&state, &params.token).await? {
        return Err(Error::Validation("invalid or expired token".into()));
    }
    Ok(Json(json!({ "verified": true })))
}

// ---------------------------------------------------------------------------
// Profile pages (epic 18)
// ---------------------------------------------------------------------------

/// Actor rows older than this get a background `RemoteActorFetch`
/// enqueue — the stored row is served meanwhile (best-effort
/// freshness, matching the rest of the codebase's eventual
/// consistency for remote data).
const REMOTE_REFRESH_AFTER_SECONDS: i64 = 24 * 60 * 60;

/// One `RemoteActorFetch` enqueue per stale actor per this window —
/// the flag covers the pending fetch so a burst of reads on a hot
/// stale profile can't flood the queue (the job is idempotent; the
/// dedup is queue pressure, not correctness). Matches the
/// `enqueue_verification` NX-flag convention.
const REFRESH_ENQUEUE_THROTTLE_SECONDS: usize = 300;

/// Load the profile's account row and apply the public-read rules:
/// missing, deleted and suspended accounts all read as 404.
async fn load_public_account(state: &AppState, id: i64) -> Result<account_mapper::AccountRow> {
    let account = account_mapper::load(&state.db, id)
        .await?
        .ok_or_else(|| Error::NotFound("account".into()))?;
    if account.suspended_at.is_some() {
        return Err(Error::NotFound("account".into()));
    }
    Ok(account)
}

/// For remote accounts, enqueue a `RemoteActorFetch` when the
/// `actor_cache` row is stale or absent. Never fails the read — a
/// federation hiccup shouldn't 5xx a profile page.
async fn maybe_refresh_remote(state: &AppState, account: &account_mapper::AccountRow) {
    if account.domain.is_none() || state.ap.is_none() {
        return;
    }
    let fresh: Option<chrono::DateTime<chrono::Utc>> =
        match sqlx::query_scalar("SELECT fetched_at FROM actor_cache WHERE actor_id = $1")
            .bind(&account.actor_id)
            .fetch_optional(&state.db)
            .await
        {
            Ok(fresh) => fresh,
            // A failed lookup maps to "stale" — the benign direction
            // (worst case an extra refresh job) — but the error must
            // not be swallowed silently.
            Err(e) => {
                tracing::warn!(
                    actor_id = %account.actor_id,
                    error = %e,
                    "actor_cache lookup failed — treating as stale"
                );
                None
            }
        };
    let stale = fresh
        .map(|t| (chrono::Utc::now() - t).num_seconds() > REMOTE_REFRESH_AFTER_SECONDS)
        .unwrap_or(true);
    if !stale || !claim_refresh_slot(state, &account.actor_id).await {
        return;
    }
    if let Err(e) = state
        .dispatcher
        .enqueue(
            crate::jobs::JobKind::RemoteActorFetch,
            json!({ "actor_url": account.actor_id }),
        )
        .await
    {
        tracing::warn!(actor_id = %account.actor_id, error = %e, "remote actor refresh enqueue failed");
        // The enqueue never landed — release the flag so the next
        // read retries instead of waiting out the throttle TTL.
        clear_refresh_slot(state, &account.actor_id).await;
    }
}

/// Claim the throttle flag for `actor_id`: `true` when this caller
/// owns the slot and should enqueue; `false` when another recent read
/// holds it or Redis is unavailable — the dispatcher lives on the
/// same Redis, so a down pool means the enqueue would fail anyway.
async fn claim_refresh_slot(state: &AppState, actor_id: &str) -> bool {
    let flag = state.redis.key(&format!("actor-refresh:{actor_id}"));
    let mut conn = match state.redis.conn().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(
                actor_id,
                error = %e,
                "redis unavailable — skipping remote actor refresh"
            );
            return false;
        }
    };
    match redis::cmd("SET")
        .arg(&flag)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(REFRESH_ENQUEUE_THROTTLE_SECONDS)
        .query_async::<Option<String>>(&mut *conn)
        .await
    {
        Ok(claimed) => claimed.is_some(),
        Err(e) => {
            tracing::warn!(actor_id, error = %e, "remote actor refresh throttle check failed");
            false
        }
    }
}

/// Release the throttle flag — used after a failed enqueue so the next
/// read retries instead of waiting out the TTL. Best-effort.
async fn clear_refresh_slot(state: &AppState, actor_id: &str) {
    let flag = state.redis.key(&format!("actor-refresh:{actor_id}"));
    let Ok(mut conn) = state.redis.conn().await else {
        return;
    };
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(&flag).query_async(&mut *conn).await;
}

/// `GET /api/v1/accounts/{id}` — the public Account entity behind the
/// profile page header (avatar, banner, counters, verified fields,
/// joined date, `locked`/`bot`, `moved`).
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{id}",
    tag = "accounts",
    params(("id" = String, Path, description = "Account id")),
    responses(
        (status = 200, description = "The account"),
        (status = 404, description = "Missing, deleted or suspended account", body = crate::error::ErrorBody),
    )
)]
pub async fn get_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    _auth: MaybeAuth,
) -> Result<Json<Value>> {
    Ok(Json(public_account_entity(&state, id).await?))
}

/// The public Account entity for `id` — the exact payload of
/// `GET /api/v1/accounts/{id}` (public-read rules, bounded remote
/// refresh, `last_status_at`). Shared with [`lookup`] so a handle
/// resolves to precisely what the numeric-id route serves.
async fn public_account_entity(state: &AppState, id: i64) -> Result<Value> {
    let account = load_public_account(state, id).await?;
    maybe_refresh_remote(state, &account).await;
    let base_url = crate::activitypub::federation_base_url(&state.config());
    let mut entity =
        account_mapper::account_entity(&state.db, &state.config(), &base_url, &account).await?;
    let last_status: Option<chrono::NaiveDate> = sqlx::query_scalar(
        "SELECT max(created_at)::date FROM statuses \
         WHERE account_id = $1 AND NOT tombstone",
    )
    .bind(account.id)
    .fetch_one(&state.db)
    .await?;
    entity["last_status_at"] = last_status
        .map(|d| json!(d.to_string()))
        .unwrap_or(Value::Null);
    Ok(entity)
}

/// `GET /api/v1/accounts/lookup?acct=` — resolve a handle to the same
/// Account entity `GET /accounts/{id}` serves (epic 25). Anonymous-
/// capable like the profile reads (not gated by
/// `search.unauthenticated_enabled`); counted against the `search`
/// bucket at the router.
///
/// Accepted `acct` shapes: `user`, `@user`, `user@domain`,
/// `@user@domain`; a domain equal to `instance.domain` normalizes to
/// the bare local handle (local rows carry `domain IS NULL`).
///
/// Error contract the SPA relies on: malformed or missing `acct` →
/// 422; a local or DB-known miss → 404; an unknown remote handle →
/// **401 only when sign-in is the missing gate** (anonymous +
/// `search.remote_fetch` on + federation up — the SPA renders a
/// sign-in hint) and 404 when remote resolution is disabled or
/// federation is down (signing in cannot help). Anonymous requests
/// never reach WebFinger/`lookup_fqn`.
#[utoipa::path(
    get,
    path = "/api/v1/accounts/lookup",
    tag = "accounts",
    params(("acct" = String, Query, description = "Handle: user, @user, user@domain or @user@domain")),
    responses(
        (status = 200, description = "The account"),
        (status = 401, description = "Sign-in would allow a remote lookup", body = crate::error::ErrorBody),
        (status = 404, description = "Missing, deleted or suspended account", body = crate::error::ErrorBody),
        (status = 422, description = "Missing or malformed acct", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn lookup(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let acct = params
        .get("acct")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Validation("acct is required".into()))?;
    // `user@<our-domain>` is the local account — normalize to the
    // bare handle *before* full parsing: local rows carry
    // `domain IS NULL`, and the instance domain may fail the
    // `plausible_domain` grammar anyway (`localhost` on dev boxes).
    // Normalizing first also guarantees we never WebFinger our own
    // host.
    let body = acct.strip_prefix('@').unwrap_or(acct);
    let normalized = match body.rsplit_once('@') {
        Some((user, d)) if d.eq_ignore_ascii_case(&state.config().instance.domain) => {
            user.to_string()
        }
        _ => acct.to_string(),
    };
    let (username, domain) = crate::search::params::parse_acct(&normalized)?;

    let id: Option<i64> = match &domain {
        None => {
            sqlx::query_scalar(
                "SELECT id FROM accounts \
                 WHERE lower(username) = lower($1) AND domain IS NULL AND deleted_at IS NULL",
            )
            .bind(&username)
            .fetch_optional(&state.db)
            .await?
        }
        Some(d) => {
            sqlx::query_scalar(
                "SELECT id FROM accounts \
                 WHERE lower(username) = lower($1) AND lower(domain) = lower($2) \
                   AND deleted_at IS NULL",
            )
            .bind(&username)
            .bind(d)
            .fetch_optional(&state.db)
            .await?
        }
    };
    if let Some(id) = id {
        // Same render path as `get_account` — no search-visibility
        // filtering (a muted/blocked account must stay reachable so
        // the viewer can unmute it).
        return Ok(Json(public_account_entity(&state, id).await?));
    }

    let Some(domain) = domain else {
        return Err(Error::NotFound("account".into()));
    };
    // Unknown remote handle: a remote fetch needs the same gates as
    // search resolution — authenticated + `remote_fetch` + federation.
    let fetch_enabled = state.config().search.remote_fetch && state.ap.is_some();
    if !fetch_enabled {
        return Err(Error::NotFound("account".into()));
    }
    let Some(_authed) = &auth.0 else {
        // Authentication is the only missing gate — the SPA prompts
        // sign-in rather than rendering "not found".
        return Err(Error::Unauthorized);
    };
    match crate::search::resolve::lookup_fqn(&state, &username, &domain).await? {
        Some(id) => Ok(Json(public_account_entity(&state, id).await?)),
        None => Err(Error::NotFound("account".into())),
    }
}

/// `GET /api/v1/accounts/{id}/statuses` — the profile's content tabs:
/// posts (with `exclude_replies`/`exclude_reblogs` flags), media
/// (`only_media=true`) and the pinned list (`pinned=true`).
#[utoipa::path(
    get,
    path = "/api/v1/accounts/{id}/statuses",
    tag = "accounts",
    params(
        ("id" = String, Path, description = "Account id"),
        ("only_media" = Option<bool>, Query, description = "Only statuses with media"),
        ("exclude_replies" = Option<bool>, Query, description = "Hide replies (self-replies stay)"),
        ("exclude_reblogs" = Option<bool>, Query, description = "Hide boosts"),
        ("pinned" = Option<bool>, Query, description = "Only pinned statuses"),
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of statuses, newest first (Link header carries cursors)"),
        (status = 404, description = "Missing, deleted or suspended account", body = crate::error::ErrorBody),
    )
)]
pub async fn account_statuses(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    auth: MaybeAuth,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config(), a, "read:statuses")?;
    }
    let account = load_public_account(&state, id).await?;
    let scope = TimelineScope::Account {
        account_id: account.id,
    };
    let viewer = auth.0.as_ref().map(|a| a.account_id);
    let rules = match viewer {
        Some(v) => {
            crate::filters::rules::load_active(&state.db, v, crate::filters::FilterContext::Account)
                .await?
        }
        None => Vec::new(),
    };
    let filters = Filters {
        viewer,
        only_media: super::timelines::truthy(&params, "only_media"),
        exclude_replies: super::timelines::truthy(&params, "exclude_replies"),
        exclude_reblogs: super::timelines::truthy(&params, "exclude_reblogs"),
        hide: crate::filters::apply::hide_clause(&rules),
        ..Default::default()
    };
    let page = paginate::parse(&params);
    let pinned = super::timelines::truthy(&params, "pinned");
    let rows = if pinned {
        // Pin order is `status_pins.idx`, not status id — resolve the
        // ids first, then re-validate visibility through the shared
        // WHERE so a pinned followers-only post still hides from
        // strangers. The pin set is capped (`max_pinned_posts`), so
        // Mastodon returns it as one list: no LIMIT, no Link cursors.
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT status_id FROM status_pins WHERE account_id = $1 ORDER BY idx, status_id",
        )
        .bind(account.id)
        .fetch_all(&state.db)
        .await?;
        crate::timelines::query::fetch_by_ids(&state.db, &scope, &filters, &ids).await?
    } else {
        crate::timelines::query::fetch(&state.db, &scope, &filters, &page).await?
    };
    let ctx = crate::posts::PostContext::new(&state);
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(crate::posts::entity::status_entity(&ctx, row, viewer).await?);
    }
    crate::filters::apply::attach_filtered(&state.db, &rules, &rows, &mut out).await?;
    let headers = if pinned {
        HeaderMap::new()
    } else {
        let cursors = crate::timelines::query::cursors(&rows);
        paginate::headers(&uri, &params, &cursors, page.limit)
    };
    Ok((headers, Json(json!(out))))
}
