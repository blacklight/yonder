//! Mastodon Account entity mapper — built from Yonder's own tables
//! (pubby's single-user shape is the wire-format reference only).
//! First consumer: `GET /api/v1/accounts/verify_credentials`; epics
//! 09/14/16 reuse it.

use serde_json::{json, Value};
use sqlx::Row;

use crate::Result;

/// An `accounts` row mapped to the Mastodon Account entity.
pub struct AccountRow {
    pub id: i64,
    pub username: String,
    pub domain: Option<String>,
    pub actor_id: String,
    pub url: Option<String>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub locked: bool,
    pub bot: bool,
    pub discoverable: bool,
    pub avatar_path: Option<String>,
    pub banner_path: Option<String>,
    pub avatar_remote_url: Option<String>,
    pub banner_remote_url: Option<String>,
    pub avatar_blurhash: Option<String>,
    pub header_blurhash: Option<String>,
    pub profile_fields: Value,
    pub followers_count: i32,
    pub following_count: i32,
    pub statuses_count: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Load an `accounts` row by primary key.
pub async fn load(pool: &sqlx::PgPool, account_id: i64) -> Result<Option<AccountRow>> {
    let row = sqlx::query(
        "SELECT id, username, domain, actor_id, url, display_name, bio, locked, \
                bot, discoverable, avatar_path, banner_path, avatar_remote_url, \
                banner_remote_url, avatar_blurhash, header_blurhash, \
                profile_fields, followers_count, following_count, \
                statuses_count, created_at \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AccountRow {
        id: r.get("id"),
        username: r.get("username"),
        domain: r.get("domain"),
        actor_id: r.get("actor_id"),
        url: r.get("url"),
        display_name: r.get("display_name"),
        bio: r.get("bio"),
        locked: r.get("locked"),
        bot: r.get("bot"),
        discoverable: r.get("discoverable"),
        avatar_path: r.get("avatar_path"),
        banner_path: r.get("banner_path"),
        avatar_remote_url: r.get("avatar_remote_url"),
        banner_remote_url: r.get("banner_remote_url"),
        avatar_blurhash: r.get("avatar_blurhash"),
        header_blurhash: r.get("header_blurhash"),
        profile_fields: r.get("profile_fields"),
        followers_count: r.get("followers_count"),
        following_count: r.get("following_count"),
        statuses_count: r.get("statuses_count"),
        created_at: r.get("created_at"),
    }))
}

/// Public media URL for a stored key (`avatars/…`, `headers/…`) or the
/// remote URL — the remote value wins for remote rows (which never
/// carry a local key).
fn media_url(base_url: &str, local_key: &Option<String>, remote: &Option<String>) -> String {
    remote
        .clone()
        .or_else(|| {
            local_key
                .as_ref()
                .map(|k| crate::media::public_url(base_url, k))
        })
        .unwrap_or_default()
}

/// `profile_fields` → the public REST `fields` projection
/// (`{name, value(html), verified_at}`): internal `value_raw` is
/// stripped; rows from before `verified_at` get a null.
fn public_fields(profile_fields: &Value) -> Vec<Value> {
    profile_fields
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            json!({
                "name": entry.get("name").cloned().unwrap_or(Value::Null),
                "value": entry.get("value").cloned().unwrap_or(Value::Null),
                "verified_at": entry.get("verified_at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// `profile_fields` → the `source.fields` projection
/// (`{name, value: raw}`) — what the edit UI shows.
pub fn source_fields(profile_fields: &Value) -> Vec<Value> {
    profile_fields
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            json!({
                "name": entry.get("name").cloned().unwrap_or(Value::Null),
                "value": entry
                    .get("value_raw")
                    .or_else(|| entry.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Map an `accounts` row to a Mastodon Account entity. `base_url` is
/// the public federation base (media keys render to absolute URLs
/// against it). `source` (privacy etc.) is added by the caller for
/// own-account responses.
pub fn to_account(account: &AccountRow, base_url: &str) -> Value {
    let acct = match &account.domain {
        Some(domain) => format!("{}@{domain}", account.username),
        None => account.username.clone(),
    };
    // Local bios are stored raw and rendered on read; remote rows keep
    // their stored HTML `summary`.
    let note = match (&account.domain, &account.bio) {
        (None, Some(raw)) => pubby_sdk::render_bio_html(raw),
        (_, bio) => bio.clone().unwrap_or_default(),
    };
    json!({
        "id": account.id.to_string(),
        "username": account.username,
        "acct": acct,
        "display_name": account.display_name.clone().unwrap_or_else(|| account.username.clone()),
        "note": note,
        "url": account.url.clone().unwrap_or_else(|| account.actor_id.clone()),
        "uri": account.actor_id,
        "avatar": media_url(base_url, &account.avatar_path, &account.avatar_remote_url),
        "avatar_static": media_url(base_url, &account.avatar_path, &account.avatar_remote_url),
        "header": media_url(base_url, &account.banner_path, &account.banner_remote_url),
        "header_static": media_url(base_url, &account.banner_path, &account.banner_remote_url),
        "locked": account.locked,
        "bot": account.bot,
        "group": false,
        "discoverable": account.discoverable,
        "noindex": false,
        "created_at": account.created_at.to_rfc3339(),
        "last_status_at": Value::Null,
        "followers_count": account.followers_count,
        "following_count": account.following_count,
        "statuses_count": account.statuses_count,
        "fields": public_fields(&account.profile_fields),
        "emojis": [],
        "yonder": {
            "avatar_blurhash": account.avatar_blurhash,
            "header_blurhash": account.header_blurhash,
        },
    })
}
