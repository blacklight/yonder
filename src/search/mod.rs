//! Search (accounts, statuses, hashtags) and remote fetch resolution.
//!
//! `GET /api/v2/search` and `GET /api/v1/search` share one pipeline:
//! [`params`] classifies `q`, the domain modules match it against the
//! trigram-indexed tables, [`resolve`] handles remote FQN/URL
//! lookups, and [`render`] emits the Mastodon response shape.
//!
//! Autocomplete is not a separate endpoint: the frontend calls
//! `/api/v2/search` with `type`, `limit=5` and `resolve=false` —
//! account/tag matching runs in prefix mode there.

pub mod accounts;
pub mod hashtags;
pub mod index;
pub mod params;
pub mod resolve;
pub mod statuses;

use serde_json::{json, Value};

use crate::auth::scopes::require_scope;
use crate::http::middleware::auth::AuthContext;
use crate::http::AppState;
use crate::posts::{self, PostContext, StatusRow};
use crate::users::account as account_mapper;
use crate::{Error, Result};

use hashtags::TagHit;
use params::{QueryKind, SearchParams, SearchType};
use resolve::ResolvedHit;

/// Escape `\`, `%`, `_` for a Postgres LIKE/ILIKE pattern (the
/// default escape char is backslash).
pub(crate) fn escape_like(q: &str) -> String {
    q.chars()
        .flat_map(|c| match c {
            '\\' | '%' | '_' => vec!['\\', c],
            c => vec![c],
        })
        .collect()
}

/// `%{q}%` for substring matching, `{q}%` for prefix (autocomplete)
/// mode.
pub(crate) fn like_pattern(q: &str, prefix: bool) -> String {
    let e = escape_like(q);
    if prefix {
        format!("{e}%")
    } else {
        format!("%{e}%")
    }
}

/// Unrendered results — the HTTP layer picks the v1/v2 shape.
#[derive(Default)]
pub struct RawResults {
    pub account_ids: Vec<i64>,
    pub statuses: Vec<StatusRow>,
    pub tags: Vec<TagHit>,
}

/// Gate a search request. Authenticated callers need `read:search`
/// (base `read` covers it); anonymous callers need
/// `search.unauthenticated_enabled`.
pub fn authorize(state: &AppState, caller: Option<AuthContext>) -> Result<Option<AuthContext>> {
    match &caller {
        Some(a) => require_scope(&state.config, a, "read:search")?,
        None if !state.config.search.unauthenticated_enabled => return Err(Error::Unauthorized),
        None => {}
    }
    Ok(caller)
}

/// Run the search. `caller` is `None` for anonymous requests
/// (allowed per `search.unauthenticated_enabled`; enforced by
/// [`authorize`]). Remote lookup needs the full gate chain:
/// authenticated + `resolve=true` + `search.remote_fetch` + federation
/// up.
pub async fn search(
    state: &AppState,
    caller: Option<&AuthContext>,
    params: &SearchParams,
) -> Result<RawResults> {
    let viewer = caller.map(|a| a.account_id);
    let index = index::build_index(&state.config.search);
    let kind = params::effective_kind(params, params::classify(&params.q));
    let prefix = params::is_prefix_mode(params, &kind);
    let can_resolve = caller.is_some()
        && params.resolve
        && state.config.search.remote_fetch
        && state.ap.is_some();
    let wants = |t: SearchType| params.r#type.is_none() || params.r#type == Some(t);
    // Lookups are single-hit: they answer page one only, or every
    // page would repeat the same entity.
    let first_page = params.offset == 0;

    let mut out = RawResults::default();
    match &kind {
        QueryKind::Fqn { username, domain } => {
            if wants(SearchType::Accounts) {
                out.account_ids = index
                    .search_accounts(&state.db, viewer, params, &kind)
                    .await?;
                // Remote resolve only on a local miss — the cached row
                // wins and no WebFinger is issued for known actors.
                if out.account_ids.is_empty() && can_resolve && first_page {
                    if let Some(id) = resolve::lookup_fqn(state, username, domain).await? {
                        if accounts::visible(&state.db, viewer, id).await? {
                            out.account_ids.push(id);
                        }
                    }
                }
            }
            // FQN + type=statuses/hashtags is empty — the query can
            // only name an account.
        }
        QueryKind::Url(url) => {
            let hit = if first_page {
                resolve::lookup_url(state, url, can_resolve).await?
            } else {
                None
            };
            match hit {
                Some(ResolvedHit::Account(id)) if wants(SearchType::Accounts) => {
                    if accounts::visible(&state.db, viewer, id).await? {
                        out.account_ids.push(id);
                    }
                }
                Some(ResolvedHit::Status(id)) if wants(SearchType::Statuses) => {
                    if let Some(row) = statuses::visible_row(&state.db, viewer, id).await? {
                        out.statuses.push(row);
                    }
                }
                _ => {}
            }
        }
        QueryKind::Hashtag(tag) => {
            if wants(SearchType::Hashtags) {
                out.tags = index
                    .search_hashtags(&state.db, viewer, tag, prefix, params.limit, params.offset)
                    .await?;
            }
        }
        QueryKind::Text(_) => {
            if wants(SearchType::Accounts) {
                out.account_ids = index
                    .search_accounts(&state.db, viewer, params, &kind)
                    .await?;
            }
            if wants(SearchType::Statuses) {
                out.statuses = index
                    .search_statuses(&state.db, viewer, params, &kind)
                    .await?;
            }
            if wants(SearchType::Hashtags) {
                // Text searches match tag names exactly; prefix
                // matching only fires in autocomplete mode.
                out.tags = index
                    .search_hashtags(
                        &state.db,
                        viewer,
                        &params.q,
                        prefix,
                        params.limit,
                        params.offset,
                    )
                    .await?;
            }
        }
    }
    Ok(out)
}

/// Render [`RawResults`] to the response body. `v1` picks the
/// Mastodon v1 hashtag shape (plain strings); v2 emits Tag objects.
pub async fn render(
    state: &AppState,
    results: &RawResults,
    viewer: Option<i64>,
    v1: bool,
) -> Result<Value> {
    let ctx = PostContext::new(state);
    let base_url = ctx.base_url();

    let mut accounts = Vec::with_capacity(results.account_ids.len());
    for account in account_mapper::load_many(&state.db, &results.account_ids).await? {
        let mut entity = account_mapper::to_account(&account, &base_url, &state.config);
        // `to_account`'s CONTRACT: callers fill `moved` themselves.
        entity["moved"] =
            crate::users::moves::moved_account_json(&state.db, &account, &base_url, &state.config)
                .await;
        accounts.push(entity);
    }

    let mut statuses = Vec::with_capacity(results.statuses.len());
    for status in &results.statuses {
        statuses.push(posts::entity::status_entity(&ctx, status, viewer).await?);
    }

    let hashtags: Vec<Value> = results
        .tags
        .iter()
        .map(|t| {
            if v1 {
                json!(t.name)
            } else {
                hashtags::tag_entity(&base_url, t)
            }
        })
        .collect();

    Ok(json!({
        "accounts": accounts,
        "statuses": statuses,
        "hashtags": hashtags,
    }))
}
