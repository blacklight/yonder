//! Postgres-backed [`ActivityPubStorage`] — the adapter Pubby calls on
//! every inbox, outbox and dereference code path.
//!
//! The trait is synchronous (the async variant is still an upstream
//! follow-up), so every method bridges to sqlx through
//! `block_in_place` + `Handle::block_on`. **This requires the
//! multi-thread tokio runtime** — `block_in_place` panics on
//! `current_thread`. The HTTP server, the job worker and the federation
//! integration tests all run `rt-multi-thread`.
//!
//! Table mapping (the schema was designed Pubby-shaped in epics 02/04):
//!
//! - followers / follow requests → `follows` / `follow_requests`
//!   (joined on `accounts.actor_id`; remote actors are upserted into
//!   `accounts` first so the FK columns always resolve)
//! - interactions → `interactions` + `interaction_mentions`
//! - activities → `activities` (`local` distinguishes outbound records
//!   from inbound dedupe rows)
//! - remote actor cache → `actor_cache`
//! - quote authorizations → `quote_authorizations` (migration 0009)

use std::collections::HashSet;
use std::future::Future;

use chrono::{DateTime, Utc};
use pubby_sdk::{
    ActivityPubError, ActivityPubStorage, FollowRequest, Follower, Interaction, InteractionStatus,
    InteractionType,
};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};

/// The crate-private result type for storage methods.
type Result<T> = std::result::Result<T, ActivityPubError>;

fn db_err(e: impl std::fmt::Display) -> ActivityPubError {
    ActivityPubError::General(format!("storage error: {e}"))
}

/// Drive an async future to completion from a sync trait method.
///
/// `block_in_place` frees the tokio worker slot while
/// `Handle::block_on` runs the sqlx future on the current thread.
fn sync_block<F: Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Postgres `ActivityPubStorage` — one struct wrapping the pool.
pub struct PgActivityPubStorage {
    pool: PgPool,
}

impl PgActivityPubStorage {
    /// Create the adapter over `pool`.
    pub fn new(pool: PgPool) -> Self {
        PgActivityPubStorage { pool }
    }

    /// The underlying pool — the rest of the module queries through it.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// The `accounts.id` of the *local* account (`domain IS NULL`) with
/// `actor_id`, if any.
async fn local_account_id_for(pool: &PgPool, actor_id: &str) -> Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM accounts WHERE actor_id = $1 AND domain IS NULL")
        .bind(actor_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)
}

/// Extract a flat string field out of a remote actor document.
pub(crate) fn doc_str(doc: &Value, key: &str) -> Option<String> {
    match doc.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(m)) => m.get("id").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// The host part of an actor URL (best-effort).
fn url_domain(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
}

/// Bounds on the remote `attachment` → `profile_fields` extraction:
/// the local-edit limits (`content.max_profile_fields` etc.) don't
/// apply to inbound documents, but a hostile or broken actor doc
/// must not bloat the JSONB column.
const REMOTE_PROFILE_FIELDS_CAP: usize = 32;
const REMOTE_FIELD_NAME_CHARS: usize = 255;
const REMOTE_FIELD_VALUE_CHARS: usize = 4096;

/// Remote actor `attachment[]` `PropertyValue` entries → the
/// `profile_fields` JSONB shape. `None` when the document carries no
/// `attachment` array — the caller treats absent as "leave stored
/// fields alone" (a partial doc must not clobber), while a
/// present-but-empty array clears them (the remote removed its
/// fields). `value` arrives pre-rendered HTML — the same wire shape
/// local entries store; `verified_at` is carried over from
/// `previous` for unchanged (name, value) pairs — a refresh that
/// didn't touch the fields must not wipe our own verification
/// stamps (a remote-claimed stamp is never trusted: the doc shape
/// carries none).
fn remote_profile_fields(actor_data: &Value, previous: &[Value]) -> Option<Value> {
    let attachments = actor_data.get("attachment")?.as_array()?;
    let previous_stamp = |name: &str, value: &str| {
        previous
            .iter()
            .find(|p| {
                p.get("name").and_then(Value::as_str) == Some(name)
                    && p.get("value").and_then(Value::as_str) == Some(value)
            })
            .and_then(|p| p.get("verified_at").cloned())
            .unwrap_or(Value::Null)
    };
    Some(Value::Array(
        attachments
            .iter()
            .filter(|a| match a.get("type").and_then(|t| t.as_str()) {
                // `type` is often omitted; when present accept any
                // `PropertyValue` spelling (plain, `schema:`-prefixed
                // or the `http://schema.org#PropertyValue` IRI older
                // Mastodon/Friendica emit).
                None => true,
                Some(t) => t
                    .rsplit(['#', '/', ':'])
                    .next()
                    .map(|s| s.eq_ignore_ascii_case("propertyvalue"))
                    .unwrap_or(false),
            })
            .filter_map(|a| {
                let name = a.get("name")?.as_str()?.trim();
                let value = a.get("value")?.as_str()?;
                if name.is_empty() || value.is_empty() {
                    return None;
                }
                let name = name
                    .chars()
                    .take(REMOTE_FIELD_NAME_CHARS)
                    .collect::<String>();
                let value = value
                    .chars()
                    .take(REMOTE_FIELD_VALUE_CHARS)
                    .collect::<String>();
                Some(json!({
                    "name": name,
                    "value": value,
                    "verified_at": previous_stamp(&name, &value),
                }))
            })
            .take(REMOTE_PROFILE_FIELDS_CAP)
            .collect(),
    ))
}

/// Upsert a remote actor into `accounts`, returning its id.
///
/// `actor_data` may be partial (e.g. the shape carried by a follower
/// record); whatever fields are present are merged, and existing
/// richer data wins over an empty refresh.
async fn upsert_remote_account(
    pool: &PgPool,
    actor_id: &str,
    actor_data: &Value,
) -> Result<Option<i64>> {
    let domain = url_domain(actor_id);
    let username = doc_str(actor_data, "preferredUsername")
        .or_else(|| {
            actor_id
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .map(String::from)
        })
        .unwrap_or_else(|| actor_id.to_string());
    let url = doc_str(actor_data, "url").unwrap_or_else(|| actor_id.to_string());
    let inbox = doc_str(actor_data, "inbox").unwrap_or_default();
    let shared_inbox = actor_data
        .get("endpoints")
        .and_then(|e| e.get("sharedInbox"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let outbox = doc_str(actor_data, "outbox").unwrap_or_default();
    let public_key = actor_data
        .get("publicKey")
        .and_then(|k| k.get("publicKeyPem"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let display_name = doc_str(actor_data, "name");
    let summary = doc_str(actor_data, "summary");
    let bot = actor_data
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| matches!(t, "Service" | "Application"))
        .unwrap_or(false);
    let avatar = actor_data
        .get("icon")
        .and_then(|i| i.get("url"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let banner = actor_data
        .get("image")
        .and_then(|i| i.get("url"))
        .and_then(|v| v.as_str())
        .map(String::from);
    // Best-effort remote counters (epic 14): Mastodon-shaped actor docs
    // publish `followersCount`/`followingCount`/`statusesCount`; absent
    // fields leave the columns untouched.
    let count = |key: &str| {
        actor_data
            .get(key)
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
            .map(|n| n.clamp(0, i32::MAX as i64) as i32)
    };
    let followers_count = count("followersCount");
    let following_count = count("followingCount");
    let statuses_count = count("statusesCount");
    // The stamp carry-over needs the stored list — only fetched when
    // the doc actually projects fields.
    let previous_fields: Vec<Value> = if actor_data.get("attachment").is_some() {
        sqlx::query_scalar::<_, Value>("SELECT profile_fields FROM accounts WHERE actor_id = $1")
            .bind(actor_id)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let profile_fields = remote_profile_fields(actor_data, &previous_fields);

    let result = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO accounts (
            domain, username, actor_id, url, inbox, shared_inbox,
            outbox_url, public_key, actor_data, display_name, bio, bot,
            avatar_remote_url, banner_remote_url,
            followers_count, following_count, statuses_count,
            profile_fields
        ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,
                  COALESCE($15, 0), COALESCE($16, 0), COALESCE($17, 0),
                  COALESCE($18, '[]'::jsonb))
        ON CONFLICT (actor_id) DO UPDATE SET
            url = EXCLUDED.url,
            inbox = EXCLUDED.inbox,
            shared_inbox = EXCLUDED.shared_inbox,
            outbox_url = EXCLUDED.outbox_url,
            public_key = COALESCE(EXCLUDED.public_key, accounts.public_key),
            followers_count = COALESCE($15, accounts.followers_count),
            following_count = COALESCE($16, accounts.following_count),
            statuses_count = COALESCE($17, accounts.statuses_count),
            actor_data = CASE WHEN EXCLUDED.actor_data = '{}'::jsonb
                THEN accounts.actor_data ELSE EXCLUDED.actor_data END,
            display_name = COALESCE(EXCLUDED.display_name, accounts.display_name),
            bio = COALESCE(EXCLUDED.bio, accounts.bio),
            avatar_remote_url = COALESCE(EXCLUDED.avatar_remote_url, accounts.avatar_remote_url),
            banner_remote_url = COALESCE(EXCLUDED.banner_remote_url, accounts.banner_remote_url),
            profile_fields = COALESCE($18, accounts.profile_fields),
            updated_at = now()
        RETURNING id
        "#,
    )
    .bind(&domain)
    .bind(username)
    .bind(actor_id)
    .bind(url)
    .bind(inbox)
    .bind(shared_inbox)
    .bind(outbox)
    .bind(public_key)
    .bind(actor_data)
    .bind(display_name)
    .bind(summary)
    .bind(bot)
    .bind(avatar)
    .bind(banner)
    .bind(followers_count)
    .bind(following_count)
    .bind(statuses_count)
    .bind(&profile_fields)
    .fetch_one(pool)
    .await
    .map(Some)
    .map_err(db_err);
    // Epic 20 — actor-level `Emoji` tags (bio/display-name emoji)
    // recorded best-effort, same as status-level tags in
    // `posts::remote`. Never fails the upsert.
    if result.as_ref().map(|o| o.is_some()).unwrap_or(false) {
        if let Some(domain) = domain {
            for tag in actor_data
                .get("tag")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if tag.get("type").and_then(|t| t.as_str()) != Some("Emoji") {
                    continue;
                }
                let name = tag.get("name").and_then(|v| v.as_str());
                let url = tag
                    .get("icon")
                    .and_then(|i| i.get("url"))
                    .and_then(|v| v.as_str())
                    .or_else(|| tag.get("icon").and_then(|v| v.as_str()));
                if let (Some(name), Some(url)) = (name, url) {
                    let code = name.trim_matches(':');
                    if let Err(e) =
                        crate::emoji::model::upsert_remote(pool, code, &domain, url).await
                    {
                        tracing::warn!(code, domain, error = %e, "actor emoji upsert failed");
                    }
                }
            }
        }
    }
    // A `movedTo` claim travels with the actor document, so a plain
    // refresh is how a move performed on the source instance reaches
    // us when no `Move` activity was seen. Best-effort — logs its own
    // failures.
    if result.is_ok() {
        sync_remote_move(pool, actor_id, actor_data).await;
    }
    result
}

/// Sync `accounts.moved_to_account_id` from a remote actor document's
/// `movedTo`. The move is honored only when the target's `alsoKnownAs`
/// names the origin — the same contract inbound `Move` activities are
/// verified against (`users::moves::migrate_remote`; for a *local*
/// target the aliases live in `account_aliases`). A document without
/// `movedTo` clears the marker — but only when the document looks
/// complete (`inbox` present), so a partial record can't clobber a
/// verified move. Never fails the caller's upsert.
pub(crate) async fn sync_remote_move(pool: &PgPool, actor_id: &str, actor_data: &Value) {
    if let Err(e) = sync_remote_move_inner(pool, actor_id, actor_data).await {
        tracing::warn!(actor = %actor_id, error = %e, "remote move sync failed");
    }
}

async fn sync_remote_move_inner(
    pool: &PgPool,
    actor_id: &str,
    actor_data: &Value,
) -> crate::Result<()> {
    let target = match doc_str(actor_data, "movedTo") {
        Some(t) if t.is_empty() || t == actor_id => return Ok(()),
        Some(t) => t,
        None => {
            if actor_data.get("inbox").is_some() {
                sqlx::query(
                    "UPDATE accounts SET moved_to_account_id = NULL, updated_at = now() \
                     WHERE actor_id = $1 AND domain IS NOT NULL \
                     AND moved_to_account_id IS NOT NULL",
                )
                .bind(actor_id)
                .execute(pool)
                .await?;
            }
            return Ok(());
        }
    };
    let row: Option<(i64, bool, Value)> =
        sqlx::query_as("SELECT id, domain IS NULL, actor_data FROM accounts WHERE actor_id = $1")
            .bind(&target)
            .fetch_optional(pool)
            .await?;
    let Some((target_id, target_local, target_data)) = row else {
        // Unknown target — `RemoteActorFetch`'s `ensure_move_target`
        // resolves it before the upsert; other callers leave the move
        // unmarked until a refresh brings the target row in.
        tracing::debug!(actor = %actor_id, %target, "movedTo target not resolved locally");
        return Ok(());
    };
    let verified = if target_local {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM account_aliases WHERE account_id = $1 AND uri = $2)",
        )
        .bind(target_id)
        .bind(actor_id)
        .fetch_one(pool)
        .await?
    } else {
        str_list(target_data.get("alsoKnownAs"))
            .iter()
            .any(|u| u == actor_id)
    };
    if verified {
        sqlx::query(
            "UPDATE accounts SET moved_to_account_id = $2, moved_at = now(), updated_at = now() \
             WHERE actor_id = $1 AND domain IS NOT NULL \
             AND moved_to_account_id IS DISTINCT FROM $2",
        )
        .bind(actor_id)
        .bind(target_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Resolve the target account for a follower/request record: the local
/// `accounts.id` matching `target_actor_id` (actor URL or object id).
/// `None` (unassigned/legacy targets) resolves to nothing.
async fn resolve_target(pool: &PgPool, target_actor_id: &str) -> Result<Option<i64>> {
    if target_actor_id.is_empty() {
        return Ok(None);
    }
    local_account_id_for(pool, target_actor_id).await
}

/// Serialize a `Map<String, Value>` back to a `Value`.
fn map_value(m: &Map<String, Value>) -> Value {
    Value::Object(m.clone())
}

/// JSON string-list field → `Vec<String>` (single strings wrap).
fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

impl ActivityPubStorage for PgActivityPubStorage {
    // ---------- Followers ----------

    fn store_follower(&self, follower: &Follower) -> Result<()> {
        sync_block(async {
            let actor_data = map_value(&follower.actor_data);
            let Some(remote_id) =
                upsert_remote_account(&self.pool, &follower.actor_id, &actor_data).await?
            else {
                return Ok(());
            };
            let Some(target_id) = resolve_target(&self.pool, &follower.target_actor_id).await?
            else {
                tracing::warn!(
                    actor = %follower.actor_id,
                    target = %follower.target_actor_id,
                    "dropping follower for unknown local target"
                );
                return Ok(());
            };
            sqlx::query(
                r#"
                INSERT INTO follows (
                    account_id, target_account_id, inbox, shared_inbox,
                    actor_data, created_at, updated_at
                ) VALUES ($1,$2,$3,$4,$5,COALESCE($6, now()),now())
                ON CONFLICT (account_id, target_account_id) DO UPDATE SET
                    inbox = EXCLUDED.inbox,
                    shared_inbox = EXCLUDED.shared_inbox,
                    actor_data = EXCLUDED.actor_data,
                    updated_at = now()
                "#,
            )
            .bind(remote_id)
            .bind(target_id)
            .bind(&follower.inbox)
            .bind(&follower.shared_inbox)
            .bind(&actor_data)
            .bind(follower.followed_at)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn remove_follower(&self, actor_id: &str, target_actor_id: &str) -> Result<()> {
        sync_block(async {
            if target_actor_id.is_empty() {
                sqlx::query(
                    "DELETE FROM follows WHERE account_id = \
                     (SELECT id FROM accounts WHERE actor_id = $1)",
                )
                .bind(actor_id)
                .execute(&self.pool)
                .await
            } else {
                sqlx::query(
                    "DELETE FROM follows WHERE \
                     account_id = (SELECT id FROM accounts WHERE actor_id = $1) AND \
                     target_account_id = (SELECT id FROM accounts WHERE actor_id = $2)",
                )
                .bind(actor_id)
                .bind(target_actor_id)
                .execute(&self.pool)
                .await
            }
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn get_followers(&self, actor_id: Option<&str>) -> Result<Vec<Follower>> {
        sync_block(async {
            let rows = sqlx::query(
                r#"
                SELECT a.actor_id, f.inbox, f.shared_inbox, f.actor_data,
                       f.created_at AS followed_at, t.actor_id AS target_actor_id
                FROM follows f
                JOIN accounts a ON a.id = f.account_id
                JOIN accounts t ON t.id = f.target_account_id
                WHERE ($1::text IS NULL OR t.actor_id = $1)
                ORDER BY f.created_at DESC
                "#,
            )
            .bind(actor_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(rows
                .into_iter()
                .map(|r| Follower {
                    actor_id: r.get("actor_id"),
                    inbox: r.get("inbox"),
                    shared_inbox: r.get("shared_inbox"),
                    followed_at: r.get("followed_at"),
                    actor_data: r
                        .get::<Value, _>("actor_data")
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    target_actor_id: r.get("target_actor_id"),
                })
                .collect())
        })
    }

    fn get_followers_of_targets(&self, target_ids: &[String]) -> Result<Vec<Follower>> {
        sync_block(async {
            let rows = sqlx::query(
                r#"
                SELECT a.actor_id, f.inbox, f.shared_inbox, f.actor_data,
                       f.created_at AS followed_at, t.actor_id AS target_actor_id
                FROM follows f
                JOIN accounts a ON a.id = f.account_id
                JOIN accounts t ON t.id = f.target_account_id
                WHERE t.actor_id = ANY($1)
                ORDER BY f.created_at DESC
                "#,
            )
            .bind(target_ids)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(rows
                .into_iter()
                .map(|r| Follower {
                    actor_id: r.get("actor_id"),
                    inbox: r.get("inbox"),
                    shared_inbox: r.get("shared_inbox"),
                    followed_at: r.get("followed_at"),
                    actor_data: r
                        .get::<Value, _>("actor_data")
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    target_actor_id: r.get("target_actor_id"),
                })
                .collect())
        })
    }

    fn get_following(&self, actor_id: &str, limit: usize, offset: usize) -> Result<Vec<String>> {
        sync_block(async {
            sqlx::query_scalar::<_, String>(
                r#"
                SELECT t.actor_id
                FROM follows f
                JOIN accounts a ON a.id = f.account_id
                JOIN accounts t ON t.id = f.target_account_id
                WHERE a.actor_id = $1
                ORDER BY f.created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(actor_id)
            .bind(limit.max(1) as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)
        })
    }

    // ---------- Follow requests ----------

    fn store_follow_request(&self, request: &FollowRequest) -> Result<()> {
        sync_block(async {
            let actor_data = map_value(&request.actor_data);
            let activity = map_value(&request.activity);
            let Some(remote_id) =
                upsert_remote_account(&self.pool, &request.actor_id, &actor_data).await?
            else {
                return Ok(());
            };
            let Some(target_id) = resolve_target(&self.pool, &request.target_actor_id).await?
            else {
                tracing::warn!(
                    actor = %request.actor_id,
                    target = %request.target_actor_id,
                    "dropping follow request for unknown local target"
                );
                return Ok(());
            };
            sqlx::query(
                r#"
                INSERT INTO follow_requests (
                    account_id, target_account_id, inbox, shared_inbox,
                    actor_data, activity, created_at
                ) VALUES ($1,$2,$3,$4,$5,$6,COALESCE($7, now()))
                ON CONFLICT (account_id, target_account_id) DO UPDATE SET
                    inbox = EXCLUDED.inbox,
                    shared_inbox = EXCLUDED.shared_inbox,
                    actor_data = EXCLUDED.actor_data,
                    activity = EXCLUDED.activity
                "#,
            )
            .bind(remote_id)
            .bind(target_id)
            .bind(&request.inbox)
            .bind(&request.shared_inbox)
            .bind(&actor_data)
            .bind(&activity)
            .bind(request.requested_at)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn get_follow_requests(&self, target_actor_id: Option<&str>) -> Result<Vec<FollowRequest>> {
        sync_block(async {
            let rows = sqlx::query(
                r#"
                SELECT a.actor_id, t.actor_id AS target_actor_id, r.inbox,
                       r.shared_inbox, r.actor_data, r.activity,
                       r.created_at AS requested_at
                FROM follow_requests r
                JOIN accounts a ON a.id = r.account_id
                JOIN accounts t ON t.id = r.target_account_id
                WHERE ($1::text IS NULL OR t.actor_id = $1)
                ORDER BY r.created_at DESC
                "#,
            )
            .bind(target_actor_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(rows.into_iter().map(request_row).collect())
        })
    }

    fn get_follow_request(
        &self,
        actor_id: &str,
        target_actor_id: &str,
    ) -> Result<Option<FollowRequest>> {
        sync_block(async {
            let row = sqlx::query(
                r#"
                SELECT a.actor_id, t.actor_id AS target_actor_id, r.inbox,
                       r.shared_inbox, r.actor_data, r.activity,
                       r.created_at AS requested_at
                FROM follow_requests r
                JOIN accounts a ON a.id = r.account_id
                JOIN accounts t ON t.id = r.target_account_id
                WHERE a.actor_id = $1 AND t.actor_id = $2
                "#,
            )
            .bind(actor_id)
            .bind(target_actor_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(row.map(request_row))
        })
    }

    fn remove_follow_request(&self, actor_id: &str, target_actor_id: &str) -> Result<bool> {
        sync_block(async {
            let done = sqlx::query(
                "DELETE FROM follow_requests WHERE \
                 account_id = (SELECT id FROM accounts WHERE actor_id = $1) AND \
                 target_account_id = (SELECT id FROM accounts WHERE actor_id = $2)",
            )
            .bind(actor_id)
            .bind(target_actor_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(done.rows_affected() > 0)
        })
    }

    // ---------- Interactions ----------

    fn store_interaction(&self, interaction: &Interaction) -> Result<()> {
        sync_block(async {
            let id = sqlx::query_scalar::<_, i64>(
                r#"
                INSERT INTO interactions (
                    source_actor_id, target_resource, interaction_type,
                    activity_id, object_id, content, author_name, author_url,
                    author_photo, published, status, metadata,
                    created_at, updated_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
                    COALESCE($13, now()), COALESCE($14, now()))
                ON CONFLICT (source_actor_id, target_resource, interaction_type, object_id)
                DO UPDATE SET
                    content = EXCLUDED.content,
                    author_name = EXCLUDED.author_name,
                    author_url = EXCLUDED.author_url,
                    author_photo = EXCLUDED.author_photo,
                    status = EXCLUDED.status,
                    metadata = EXCLUDED.metadata,
                    updated_at = now()
                RETURNING id
                "#,
            )
            .bind(&interaction.source_actor_id)
            .bind(&interaction.target_resource)
            .bind(interaction.interaction_type.as_str())
            .bind(&interaction.activity_id)
            .bind(&interaction.object_id)
            .bind(&interaction.content)
            .bind(&interaction.author_name)
            .bind(&interaction.author_url)
            .bind(&interaction.author_photo)
            .bind(interaction.published)
            .bind(interaction.status.as_str())
            .bind(map_value(&interaction.metadata))
            .bind(interaction.created_at)
            .bind(interaction.updated_at)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;

            sqlx::query("DELETE FROM interaction_mentions WHERE interaction_id = $1")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
            for url in &interaction.mentioned_actors {
                sqlx::query(
                    "INSERT INTO interaction_mentions (interaction_id, mentioned_actor_url) \
                     VALUES ($1,$2) ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(url)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
            }
            Ok(())
        })
    }

    fn delete_interaction(
        &self,
        source_actor_id: &str,
        target_resource: &str,
        interaction_type: InteractionType,
    ) -> Result<()> {
        sync_block(async {
            sqlx::query(
                "UPDATE interactions SET status = 'deleted', updated_at = now() \
                 WHERE source_actor_id = $1 AND target_resource = $2 \
                 AND interaction_type = $3",
            )
            .bind(source_actor_id)
            .bind(target_resource)
            .bind(interaction_type.as_str())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn delete_interaction_by_object_id(
        &self,
        source_actor_id: &str,
        object_id: &str,
    ) -> Result<bool> {
        sync_block(async {
            let done = sqlx::query(
                "UPDATE interactions SET status = 'deleted', updated_at = now() \
                 WHERE object_id = $1 AND source_actor_id = $2",
            )
            .bind(object_id)
            .bind(source_actor_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(done.rows_affected() > 0)
        })
    }

    fn get_interaction_by_object_id(
        &self,
        object_id: &str,
        status: InteractionStatus,
    ) -> Result<Option<Interaction>> {
        sync_block(async {
            let rows = sqlx::query(
                "SELECT * FROM interactions WHERE object_id = $1 AND status = $2 \
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(object_id)
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            self.hydrate_interactions(rows).await.map(|mut v| v.pop())
        })
    }

    fn get_interactions(
        &self,
        target_resource: &str,
        interaction_type: Option<InteractionType>,
        status: InteractionStatus,
    ) -> Result<Vec<Interaction>> {
        sync_block(async {
            let rows = sqlx::query(
                "SELECT * FROM interactions WHERE target_resource = $1 AND status = $2 \
                 AND ($3::text IS NULL OR interaction_type = $3) \
                 ORDER BY id DESC",
            )
            .bind(target_resource)
            .bind(status.as_str())
            .bind(interaction_type.map(|t| t.as_str().to_string()))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            self.hydrate_interactions(rows).await
        })
    }

    fn get_interactions_mentioning(
        &self,
        actor_url: &str,
        interaction_type: Option<InteractionType>,
        status: InteractionStatus,
    ) -> Result<Vec<Interaction>> {
        sync_block(async {
            let rows = sqlx::query(
                "SELECT i.* FROM interactions i \
                 JOIN interaction_mentions m ON m.interaction_id = i.id \
                 WHERE m.mentioned_actor_url = $1 AND i.status = $2 \
                 AND ($3::text IS NULL OR i.interaction_type = $3) \
                 ORDER BY i.id DESC",
            )
            .bind(actor_url)
            .bind(status.as_str())
            .bind(interaction_type.map(|t| t.as_str().to_string()))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            self.hydrate_interactions(rows).await
        })
    }

    // ---------- Activities ----------

    fn store_activity(&self, activity_id: &str, activity_data: &Value) -> Result<()> {
        sync_block(async {
            let actor = doc_str(activity_data, "actor").unwrap_or_default();
            let (object_id, object_type) = match activity_data.get("object") {
                Some(Value::String(s)) => (s.clone(), String::new()),
                Some(Value::Object(m)) => (
                    m.get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    m.get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                ),
                _ => (String::new(), String::new()),
            };
            let to_recipients = str_list(activity_data.get("to"));
            let cc_recipients = str_list(activity_data.get("cc"));
            let audience: HashSet<String> = to_recipients
                .iter()
                .chain(&cc_recipients)
                .chain(str_list(activity_data.get("audience")).iter())
                .cloned()
                .collect();
            let audience: Vec<String> = audience.into_iter().collect();
            let published = activity_data
                .get("published")
                .and_then(pubby_sdk::model::parse_dt);

            sqlx::query(
                r#"
                INSERT INTO activities (
                    activity_id, type, actor_id, object_id, object_type,
                    object, to_recipients, cc_recipients, audience, local,
                    published_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,true,$10)
                -- NB: `local` is deliberately NOT in the UPDATE list —
                -- an inbound row deduped by the inbox route (`local=false`)
                -- keeps its flag when pubby re-stores the same activity.
                ON CONFLICT (activity_id) DO UPDATE SET
                    type = EXCLUDED.type,
                    actor_id = EXCLUDED.actor_id,
                    object_id = EXCLUDED.object_id,
                    object_type = EXCLUDED.object_type,
                    object = EXCLUDED.object,
                    to_recipients = EXCLUDED.to_recipients,
                    cc_recipients = EXCLUDED.cc_recipients,
                    audience = EXCLUDED.audience,
                    published_at = EXCLUDED.published_at
                "#,
            )
            .bind(activity_id)
            .bind(
                activity_data
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .bind(actor)
            .bind(object_id)
            .bind(object_type)
            .bind(activity_data)
            .bind(&to_recipients)
            .bind(&cc_recipients)
            .bind(&audience)
            .bind(published)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn get_activities(&self, limit: usize, offset: usize) -> Result<Vec<Value>> {
        sync_block(async {
            sqlx::query_scalar::<_, Value>(
                "SELECT object FROM activities WHERE local \
                 ORDER BY published_at DESC NULLS LAST, id DESC LIMIT $1 OFFSET $2",
            )
            .bind(limit.max(1) as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)
        })
    }

    fn get_activities_for_actor(
        &self,
        actor_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Value>> {
        sync_block(async {
            sqlx::query_scalar::<_, Value>(
                "SELECT object FROM activities WHERE local AND actor_id = $1 \
                 ORDER BY published_at DESC NULLS LAST, id DESC LIMIT $2 OFFSET $3",
            )
            .bind(actor_id)
            .bind(limit.max(1) as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)
        })
    }

    // ---------- Actor cache ----------

    fn cache_remote_actor(
        &self,
        actor_id: &str,
        actor_data: &Value,
        fetched_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sync_block(async {
            sqlx::query(
                "INSERT INTO actor_cache (actor_id, actor_data, fetched_at) \
                 VALUES ($1,$2,COALESCE($3, now())) \
                 ON CONFLICT (actor_id) DO UPDATE SET \
                 actor_data = EXCLUDED.actor_data, fetched_at = EXCLUDED.fetched_at",
            )
            .bind(actor_id)
            .bind(actor_data)
            .bind(fetched_at)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn get_cached_actor(&self, actor_id: &str, max_age_seconds: f64) -> Result<Option<Value>> {
        sync_block(async {
            sqlx::query_scalar::<_, Value>(
                "SELECT actor_data FROM actor_cache WHERE actor_id = $1 \
                 AND fetched_at > now() - ($2::float8 * interval '1 second')",
            )
            .bind(actor_id)
            .bind(max_age_seconds)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)
        })
    }

    // ---------- Quote authorizations ----------

    fn store_quote_authorization(
        &self,
        authorization_id: &str,
        authorization_data: &Value,
    ) -> Result<()> {
        sync_block(async {
            sqlx::query(
                "INSERT INTO quote_authorizations (authorization_id, data) \
                 VALUES ($1,$2) \
                 ON CONFLICT (authorization_id) DO UPDATE SET data = EXCLUDED.data",
            )
            .bind(authorization_id)
            .bind(authorization_data)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            Ok(())
        })
    }

    fn get_quote_authorization(&self, authorization_id: &str) -> Result<Option<Value>> {
        sync_block(async {
            sqlx::query_scalar::<_, Value>(
                "SELECT data FROM quote_authorizations WHERE authorization_id = $1",
            )
            .bind(authorization_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)
        })
    }
}

/// Upsert a remote actor row for callers outside the trait (the
/// `RemoteActorFetch` job, `on_activity` side-effects).
pub(crate) async fn upsert_remote_actor_row(
    pool: &PgPool,
    actor_id: &str,
    actor_data: &Value,
) -> Result<Option<i64>> {
    upsert_remote_account(pool, actor_id, actor_data).await
}

/// Whether a `RemoteActorFetch` for `account_id` recently failed (the
/// 'actor' marker — suppresses re-enqueue for the stale-refresh
/// window) or the remote reported the actor gone ('actor_gone' —
/// suppresses for the gone-recheck window). `false` on lookup error:
/// the benign direction is an extra enqueue.
pub(crate) async fn fetch_markers_fresh(pool: &PgPool, account_id: i64) -> bool {
    let rows: Vec<(String, DateTime<Utc>)> = match sqlx::query_as(
        "SELECT direction, synced_at FROM remote_network_syncs \
         WHERE account_id = $1 AND direction IN ('actor', 'actor_gone')",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "fetch marker lookup failed");
            return false;
        }
    };
    let now = Utc::now();
    rows.iter().any(|(direction, t)| {
        let window = if direction == "actor_gone" {
            super::GONE_RECHECK_SECONDS
        } else {
            super::REMOTE_ACTOR_STALE_SECONDS
        };
        (now - *t).num_seconds() < window
    })
}

/// Convert a `follow_requests` joined row into a [`FollowRequest`].
fn request_row(r: sqlx::postgres::PgRow) -> FollowRequest {
    FollowRequest {
        actor_id: r.get("actor_id"),
        target_actor_id: r.get("target_actor_id"),
        inbox: r.get("inbox"),
        shared_inbox: r.get("shared_inbox"),
        actor_data: r
            .get::<Value, _>("actor_data")
            .as_object()
            .cloned()
            .unwrap_or_default(),
        activity: r
            .get::<Value, _>("activity")
            .as_object()
            .cloned()
            .unwrap_or_default(),
        requested_at: r.get("requested_at"),
    }
}

impl PgActivityPubStorage {
    /// Load `interaction_mentions` for a batch of interaction rows.
    async fn hydrate_interactions(
        &self,
        rows: Vec<sqlx::postgres::PgRow>,
    ) -> Result<Vec<Interaction>> {
        let ids: Vec<i64> = rows.iter().map(|r| r.get("id")).collect();
        let mention_rows = sqlx::query(
            "SELECT interaction_id, mentioned_actor_url FROM interaction_mentions \
             WHERE interaction_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut mentions: std::collections::HashMap<i64, Vec<String>> =
            std::collections::HashMap::new();
        for m in mention_rows {
            mentions
                .entry(m.get("interaction_id"))
                .or_default()
                .push(m.get("mentioned_actor_url"));
        }
        Ok(rows
            .into_iter()
            .map(|r| {
                let id: i64 = r.get("id");
                Interaction {
                    source_actor_id: r.get("source_actor_id"),
                    target_resource: r.get("target_resource"),
                    interaction_type: InteractionType::from_raw(
                        r.get::<String, _>("interaction_type").as_str(),
                    )
                    .unwrap_or_default(),
                    activity_id: r.get("activity_id"),
                    object_id: r.get("object_id"),
                    content: r.get("content"),
                    author_name: r.get("author_name"),
                    author_url: r.get("author_url"),
                    author_photo: r.get("author_photo"),
                    published: r.get("published"),
                    status: InteractionStatus::from_raw(r.get::<String, _>("status").as_str())
                        .unwrap_or_default(),
                    metadata: r
                        .get::<Value, _>("metadata")
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                    created_at: r.get("created_at"),
                    updated_at: r.get("updated_at"),
                    mentioned_actors: mentions.remove(&id).unwrap_or_default(),
                }
            })
            .collect())
    }
}
