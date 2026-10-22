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
    /// `users.indexable` for local accounts (`None` for remote rows,
    /// which have no `users` row).
    pub indexable: Option<bool>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Load an `accounts` row by primary key. The `users` LEFT JOIN is how
/// `noindex` gets its value for local accounts; remote rows have no
/// `users` row and keep the historical `noindex: false`.
pub async fn load(pool: &sqlx::PgPool, account_id: i64) -> Result<Option<AccountRow>> {
    let row = sqlx::query(
        "SELECT a.id, a.username, a.domain, a.actor_id, a.url, a.display_name, \
                a.bio, a.locked, a.bot, a.discoverable, a.avatar_path, \
                a.banner_path, a.avatar_remote_url, a.banner_remote_url, \
                a.avatar_blurhash, a.header_blurhash, \
                a.profile_fields, a.followers_count, a.following_count, \
                a.statuses_count, a.created_at, u.indexable \
         FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
         WHERE a.id = $1",
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
        indexable: r.get("indexable"),
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
        // Local accounts honor the search-engine opt-out; remote rows
        // carry no users.indexable and stay unmarked.
        "noindex": !account.indexable.unwrap_or(true),
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
