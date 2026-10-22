//! Statuses/posts: compose, render, edit, delete (epic 12).
//!
//! [`PostContext`] bundles the [`AppState`] collaborators the posting
//! services need (`db`, `redis`, `dispatcher`, `config`, optional `ap`)
//! so every service takes one argument; handlers stay thin.
//!
//! Visibility canonicalization: statuses *store* `public | unlisted |
//! followers_only | direct` — the redundant `private` spelling (admitted
//! by the schema CHECK and writable through user settings) is
//! normalized to `followers_only` at the write boundary, while the
//! Mastodon wire value is always `private` ([`wire_visibility`]).
//!
//! Cross-cutting invariants (see `docs/posting.md`):
//!
//! - **Ownership**: local mutations are owner-gated by
//!   `auth.account_id`; inbound remote mutations require
//!   `activity.actor` == the object's `attributedTo` ([`remote`]).
//! - **Audience authorization**: serving a status object applies
//!   [`can_see`] on the *resolved signer* — a valid HTTP signature is
//!   authentication, never authorization.
//! - **Idempotency**: status/scheduled-status creation is guarded by
//!   the atomic `status_idempotency_keys` record ([`create`]).
//! - **Scheduled-media reservation**: media attached to a scheduled
//!   post is referenced via `scheduled_status_media` so orphan cleanup
//!   never reclaims it ([`scheduled`]).
//! - **Job due-time checks**: the scheduled-publish handler re-checks
//!   `scheduled_at` under a row lock and refuses to publish early.

pub mod create;
pub mod entity;
pub mod jobs;
pub mod mentions;
pub mod polls;
pub mod preview;
pub mod remote;
pub mod render;
pub mod scheduled;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::{
    activitypub::{federation_base_url, ApContext},
    cache::Redis,
    config::Config,
    http::AppState,
    jobs::Dispatcher,
    Error, Result,
};

/// The dependencies the posting services share, built per-request
/// from [`AppState`] (or once where handlers aren't involved).
#[derive(Clone)]
pub struct PostContext {
    /// PostgreSQL pool.
    pub db: PgPool,
    /// Redis handle (job payloads, preview cache).
    pub redis: Redis,
    /// Background-job enqueue access.
    pub dispatcher: Dispatcher,
    /// Effective configuration.
    pub config: Arc<Config>,
    /// ActivityPub context — `None` when federation is disabled, in
    /// which case posting works without AP fan-out.
    pub ap: Option<ApContext>,
}

impl PostContext {
    /// Build the context from the request's shared state.
    pub fn new(state: &AppState) -> Self {
        PostContext {
            db: state.db.clone(),
            redis: state.redis.clone(),
            dispatcher: state.dispatcher.clone(),
            config: state.config.clone(),
            ap: state.ap.clone(),
        }
    }

    /// Build the context inside a job handler.
    pub fn from_job(ctx: &crate::jobs::JobContext, ap: Option<ApContext>) -> Self {
        PostContext {
            db: ctx.db.clone(),
            redis: ctx.redis.clone(),
            dispatcher: crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs),
            config: ctx.config.clone(),
            ap,
        }
    }

    /// The public base URL status URIs and media URLs render against.
    pub fn base_url(&self) -> String {
        self.ap
            .as_ref()
            .map(|ap| ap.core.base_url.clone())
            .unwrap_or_else(|| federation_base_url(&self.config))
    }

    /// `uri`/`url` pair for a local status once its id is known.
    pub fn status_urls(&self, username: &str, status_id: i64) -> (String, String) {
        let base = self.base_url();
        (
            format!("{base}/ap/users/{username}/statuses/{status_id}"),
            format!("{base}/@{username}/{status_id}"),
        )
    }
}

// ---------------------------------------------------------------------------
// Visibility
// ---------------------------------------------------------------------------

/// Stored visibility values (the `statuses.visibility` CHECK set, minus
/// the redundant `private` spelling this boundary normalizes away).
pub const VISIBILITIES: &[&str] = &["public", "unlisted", "followers_only", "direct"];

/// Normalize an API-supplied visibility to its stored value: Mastodon's
/// `private` and the `followers_only` alias both store
/// `followers_only`; `unlisted`/`public`/`direct` pass through.
pub fn normalize_visibility(input: &str) -> Result<String> {
    match input {
        "public" | "unlisted" | "direct" => Ok(input.to_string()),
        "private" | "followers_only" => Ok("followers_only".to_string()),
        other => Err(Error::Validation(format!(
            "visibility must be one of: public, unlisted, private, direct (got {other:?})"
        ))),
    }
}

/// Stored visibility → the Mastodon wire value (`followers_only` and
/// legacy `private` rows both render `private`).
pub fn wire_visibility(stored: &str) -> &'static str {
    match stored {
        "followers_only" | "private" => "private",
        "public" => "public",
        "unlisted" => "unlisted",
        _ => "direct",
    }
}

/// A stored default-posting visibility (`accounts.default_visibility`)
/// to use when a request omits `visibility`. A legacy `direct` default
/// degrades to `followers_only` — `direct` requires explicit mentions a
/// fresh post may lack.
pub fn default_visibility(stored: &str) -> &'static str {
    match stored {
        "public" => "public",
        "unlisted" => "unlisted",
        _ => "followers_only",
    }
}

/// The AP `to`/`cc` pair for a stored visibility. `followers` is the
/// author's followers-collection URL; `mentions` are resolved mention
/// actor URLs.
pub fn addressing(
    visibility: &str,
    followers: &str,
    mentions: &[String],
) -> (Vec<String>, Vec<String>) {
    let public = pubby_sdk::AS_PUBLIC.to_string();
    match visibility {
        "public" => {
            let mut cc = vec![followers.to_string()];
            cc.extend(mentions.iter().cloned());
            (vec![public], cc)
        }
        "unlisted" => {
            let mut to = vec![followers.to_string()];
            to.extend(mentions.iter().cloned());
            (to, vec![public])
        }
        "direct" => (mentions.to_vec(), Vec::new()),
        // followers_only / private
        _ => {
            let mut to = vec![followers.to_string()];
            to.extend(mentions.iter().cloned());
            (to, Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// Status rows
// ---------------------------------------------------------------------------

/// The `statuses` row shape the services and entity mapper share.
#[derive(Debug)]
pub struct StatusRow {
    pub id: i64,
    pub account_id: i64,
    pub content: String,
    pub content_html: Option<String>,
    pub content_type: String,
    pub language: Option<String>,
    pub visibility: String,
    pub spoiler_text: Option<String>,
    pub sensitive: bool,
    pub local: bool,
    pub uri: Option<String>,
    pub url: Option<String>,
    pub in_reply_to_id: Option<i64>,
    pub in_reply_to_account_id: Option<i64>,
    pub reblog_of_id: Option<i64>,
    pub quote_of_id: Option<i64>,
    pub poll_id: Option<i64>,
    pub replies_count: i32,
    pub reblogs_count: i32,
    pub favourites_count: i32,
    pub quotes_count: i32,
    pub edited_at: Option<DateTime<Utc>>,
    pub tombstone: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const STATUS_COLS: &str = "id, account_id, content, content_html, content_type, language, \
     visibility, spoiler_text, sensitive, local, uri, url, in_reply_to_id, \
     in_reply_to_account_id, reblog_of_id, quote_of_id, poll_id, replies_count, \
     reblogs_count, favourites_count, quotes_count, edited_at, tombstone, \
     created_at, updated_at";

fn row_to_status(r: &sqlx::postgres::PgRow) -> StatusRow {
    use sqlx::Row;
    StatusRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        content: r.get("content"),
        content_html: r.get("content_html"),
        content_type: r.get("content_type"),
        language: r.get("language"),
        visibility: r.get("visibility"),
        spoiler_text: r.get("spoiler_text"),
        sensitive: r.get("sensitive"),
        local: r.get("local"),
        uri: r.get("uri"),
        url: r.get("url"),
        in_reply_to_id: r.get("in_reply_to_id"),
        in_reply_to_account_id: r.get("in_reply_to_account_id"),
        reblog_of_id: r.get("reblog_of_id"),
        quote_of_id: r.get("quote_of_id"),
        poll_id: r.get("poll_id"),
        replies_count: r.get("replies_count"),
        reblogs_count: r.get("reblogs_count"),
        favourites_count: r.get("favourites_count"),
        quotes_count: r.get("quotes_count"),
        edited_at: r.get("edited_at"),
        tombstone: r.get("tombstone"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

/// Load a status by primary key (`None` for tombstones unless
/// `allow_tombstone`).
pub async fn load(db: &PgPool, status_id: i64) -> Result<Option<StatusRow>> {
    let row = sqlx::query(&format!(
        "SELECT {STATUS_COLS} FROM statuses WHERE id = $1 AND NOT tombstone"
    ))
    .bind(status_id)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_status))
}

/// Load a status including tombstoned rows (delete/object-serve paths).
pub async fn load_any(db: &PgPool, status_id: i64) -> Result<Option<StatusRow>> {
    let row = sqlx::query(&format!("SELECT {STATUS_COLS} FROM statuses WHERE id = $1"))
        .bind(status_id)
        .fetch_optional(db)
        .await?;
    Ok(row.as_ref().map(row_to_status))
}

/// Load a status by its AP `uri` (remote materialization, flag objects).
pub async fn load_by_uri(db: &PgPool, uri: &str) -> Result<Option<StatusRow>> {
    let row = sqlx::query(&format!(
        "SELECT {STATUS_COLS} FROM statuses WHERE uri = $1"
    ))
    .bind(uri)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_status))
}

/// The minimal read-side visibility primitive epic 15's timelines
/// extend: the author always sees their own; `public`/`unlisted` are
/// world-readable; `followers_only` needs an accepted `follows` row
/// from viewer to author; `direct` needs a `status_mentions` row.
/// Anonymous viewers (`None`) only see public/unlisted.
pub async fn can_see(
    db: &PgPool,
    status: &StatusRow,
    viewer_account_id: Option<i64>,
) -> Result<bool> {
    if Some(status.account_id) == viewer_account_id {
        return Ok(true);
    }
    match status.visibility.as_str() {
        "public" | "unlisted" => Ok(true),
        "direct" => match viewer_account_id {
            Some(viewer) => Ok(sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM status_mentions \
                 WHERE status_id = $1 AND account_id = $2)",
            )
            .bind(status.id)
            .bind(viewer)
            .fetch_one(db)
            .await?),
            None => Ok(false),
        },
        // followers_only / private
        _ => match viewer_account_id {
            Some(viewer) => Ok(sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM follows \
                 WHERE account_id = $1 AND target_account_id = $2)",
            )
            .bind(viewer)
            .bind(status.account_id)
            .fetch_one(db)
            .await?),
            None => Ok(false),
        },
    }
}

/// Register this epic's job handlers, replacing the stubs. `ap` is
/// `Some` when federation is enabled; publish/delete degrade to local
/// work without it.
pub fn register_jobs(registry: &mut crate::jobs::Registry, ap: Option<&ApContext>) {
    jobs::register(registry, ap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_normalization() {
        assert_eq!(normalize_visibility("private").unwrap(), "followers_only");
        assert_eq!(
            normalize_visibility("followers_only").unwrap(),
            "followers_only"
        );
        assert_eq!(normalize_visibility("direct").unwrap(), "direct");
        assert!(normalize_visibility("friends").is_err());
    }

    #[test]
    fn wire_mapping() {
        assert_eq!(wire_visibility("followers_only"), "private");
        assert_eq!(wire_visibility("private"), "private");
        assert_eq!(wire_visibility("public"), "public");
        assert_eq!(wire_visibility("unlisted"), "unlisted");
    }

    #[test]
    fn addressing_matrix() {
        let f = "https://ex/ap/users/u/followers".to_string();
        let m = vec!["https://remote/users/x".to_string()];
        let (to, cc) = addressing("public", &f, &m);
        assert_eq!(to, vec![pubby_sdk::AS_PUBLIC.to_string()]);
        assert!(cc.contains(&f) && cc.contains(&m[0]));
        let (to, cc) = addressing("unlisted", &f, &m);
        assert!(to.contains(&f) && cc == vec![pubby_sdk::AS_PUBLIC.to_string()]);
        let (to, cc) = addressing("followers_only", &f, &m);
        assert!(to.contains(&f) && cc.is_empty());
        let (to, cc) = addressing("direct", &f, &m);
        assert_eq!(to, m);
        assert!(cc.is_empty());
    }
}
