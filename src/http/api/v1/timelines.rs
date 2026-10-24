//! `/api/v1/timelines/*` — home, public (local/federated), list and
//! tag timelines (epic 15).
//!
//! Handlers stay thin: scope + filters are assembled here, the read
//! itself is `timelines::{query,fanout}` and entities render through
//! `posts::entity`. All four endpoints carry the `timeline` rate-limit
//! bucket (registered at the router, outside the `authentication`
//! bucket — see `mod.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, Uri},
    Json,
};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    filters::{self as content_filters, FilterContext, Rule},
    http::{
        middleware::auth::{AuthContext, MaybeAuth},
        paginate, AppState,
    },
    posts::{self, PostContext, StatusRow},
    social,
    timelines::{self, scope::RepliesPolicy, Filters, TimelineScope},
    users::settings as user_settings,
    Result,
};

/// Mastodon's loose booleans — `true`/`1` count, everything else doesn't.
pub(crate) fn truthy(params: &HashMap<String, String>, key: &str) -> bool {
    params.get(key).is_some_and(|v| v == "true" || v == "1")
}

/// `Filters` plus the viewer's active content-filter rules for one
/// context — anonymous requests carry neither `filter_languages` nor
/// content filters (there is no user to read them from).
async fn filters(
    state: &AppState,
    auth: Option<&AuthContext>,
    params: &HashMap<String, String>,
    context: FilterContext,
) -> Result<(Filters, Vec<Rule>)> {
    let hidden_languages = match auth {
        Some(a) => {
            user_settings::load(&state.db, a.user_id)
                .await?
                .filter_languages
        }
        None => Vec::new(),
    };
    let rules = match auth {
        Some(a) => content_filters::rules::load_active(&state.db, a.account_id, context).await?,
        None => Vec::new(),
    };
    Ok((
        Filters {
            viewer: auth.map(|a| a.account_id),
            hidden_languages,
            only_media: truthy(params, "only_media"),
            hide: content_filters::apply::hide_clause(&rules),
            ..Default::default()
        },
        rules,
    ))
}

/// Render rows to Status entities, attach the epic-20 `filtered`
/// attribute for matching `warn`/`blur` rules, and emit the pagination
/// `Link` header — the shared tail of every handler.
async fn render(
    state: &AppState,
    rows: Vec<StatusRow>,
    viewer: Option<i64>,
    rules: &[Rule],
    uri: &Uri,
    params: &HashMap<String, String>,
    limit: i64,
) -> Result<(HeaderMap, Json<Value>)> {
    let ctx = PostContext::new(state);
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(posts::entity::status_entity(&ctx, row, viewer).await?);
    }
    content_filters::apply::attach_filtered(&state.db, rules, &rows, &mut out).await?;
    let cursors = timelines::query::cursors(&rows);
    let headers = paginate::headers(uri, params, &cursors, limit);
    Ok((headers, Json(json!(out))))
}

/// `GET /api/v1/timelines/home` — own + followed accounts, boosted
/// posts per `follows.show_reblogs`. Redis-indexed with SQL
/// re-validation (see `timelines::fanout`).
#[utoipa::path(
    get,
    path = "/api/v1/timelines/home",
    tag = "timelines",
    params(
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)"),
        ("only_media" = Option<bool>, Query, description = "Only statuses with media")
    ),
    responses(
        (status = 200, description = "Page of statuses, newest first (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 403, description = "Scope gate", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn home(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:statuses")?;
    let page = paginate::parse(&params);
    let (filters, rules) = filters(&state, Some(&auth), &params, FilterContext::Home).await?;
    let rows = timelines::fanout::read_home(
        &state.db,
        &state.redis,
        &state.config().timelines,
        &filters,
        &page,
    )
    .await?;
    render(
        &state,
        rows,
        Some(auth.account_id),
        &rules,
        &uri,
        &params,
        page.limit,
    )
    .await
}

/// `GET /api/v1/timelines/public` — `local=true` restricts to local
/// users; `remote=true` to remote users. Anonymous access allowed —
/// this is the logged-out landing feed.
#[utoipa::path(
    get,
    path = "/api/v1/timelines/public",
    tag = "timelines",
    params(
        ("local" = Option<bool>, Query, description = "Only local users' posts"),
        ("remote" = Option<bool>, Query, description = "Only remote users' posts"),
        ("only_media" = Option<bool>, Query, description = "Only statuses with media"),
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of public statuses, newest first (Link header carries cursors)"),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn public(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config(), a, "read:statuses")?;
    }
    let scope = if truthy(&params, "local") {
        TimelineScope::Local
    } else {
        TimelineScope::Federated {
            remote_only: truthy(&params, "remote"),
        }
    };
    let page = paginate::parse(&params);
    let (filters, rules) = filters(&state, auth.0.as_ref(), &params, FilterContext::Public).await?;
    let rows = timelines::query::fetch(&state.db, &scope, &filters, &page).await?;
    render(
        &state,
        rows,
        auth.0.as_ref().map(|a| a.account_id),
        &rules,
        &uri,
        &params,
        page.limit,
    )
    .await
}

/// `GET /api/v1/timelines/list/{id}` — statuses from the list's
/// members, honoring `replies_policy`; members of `exclusive` lists
/// are removed from the home feed instead. Foreign lists 404.
#[utoipa::path(
    get,
    path = "/api/v1/timelines/list/{id}",
    tag = "timelines",
    params(
        ("id" = String, Path, description = "List id"),
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)"),
        ("only_media" = Option<bool>, Query, description = "Only statuses with media")
    ),
    responses(
        (status = 200, description = "Page of statuses, newest first (Link header carries cursors)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
        (status = 404, description = "No such list", body = crate::error::ErrorBody),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn list_timeline(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(list_id): Path<i64>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    require_scope(&state.config(), &auth, "read:lists")?;
    let list = social::lists::get_owned(&state.db, auth.account_id, list_id).await?;
    let scope = TimelineScope::List {
        list_id: list.id,
        replies_policy: RepliesPolicy::parse(&list.replies_policy),
    };
    let page = paginate::parse(&params);
    let (filters, rules) = filters(&state, Some(&auth), &params, FilterContext::Home).await?;
    let rows = timelines::query::fetch(&state.db, &scope, &filters, &page).await?;
    render(
        &state,
        rows,
        Some(auth.account_id),
        &rules,
        &uri,
        &params,
        page.limit,
    )
    .await
}

/// `GET /api/v1/timelines/tag/{hashtag}` — public statuses carrying
/// the tag, case-insensitive. Anonymous access allowed.
#[utoipa::path(
    get,
    path = "/api/v1/timelines/tag/{hashtag}",
    tag = "timelines",
    params(
        ("hashtag" = String, Path, description = "Tag name without #"),
        ("local" = Option<bool>, Query, description = "Only local users' posts"),
        ("only_media" = Option<bool>, Query, description = "Only statuses with media"),
        ("max_id" = Option<String>, Query, description = "Return statuses older than this id"),
        ("min_id" = Option<String>, Query, description = "Return statuses immediately newer than this id"),
        ("since_id" = Option<String>, Query, description = "Alias of min_id"),
        ("limit" = Option<i64>, Query, description = "Page size (1-40, default 20)")
    ),
    responses(
        (status = 200, description = "Page of tagged statuses, newest first (Link header carries cursors)"),
        (status = 429, description = "Rate limited", body = crate::error::ErrorBody),
    )
)]
pub async fn tag(
    State(state): State<Arc<AppState>>,
    auth: MaybeAuth,
    Path(hashtag): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(HeaderMap, Json<Value>)> {
    if let Some(a) = &auth.0 {
        require_scope(&state.config(), a, "read:statuses")?;
    }
    let page = paginate::parse(&params);
    let (filters, rules) = filters(&state, auth.0.as_ref(), &params, FilterContext::Public).await?;
    let scope = TimelineScope::Tag {
        name: hashtag,
        local_only: truthy(&params, "local"),
    };
    let rows = timelines::query::fetch(&state.db, &scope, &filters, &page).await?;
    render(
        &state,
        rows,
        auth.0.as_ref().map(|a| a.account_id),
        &rules,
        &uri,
        &params,
        page.limit,
    )
    .await
}
