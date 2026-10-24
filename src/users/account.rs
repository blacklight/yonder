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
    /// Instance suspension — suspended accounts 404 on public reads.
    pub suspended_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Move target — the `moved` entity renders it nested.
    pub moved_to_account_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The `accounts`/`users` projection [`load`] and [`load_many`] share.
const ACCOUNT_COLS: &str = "a.id, a.username, a.domain, a.actor_id, a.url, a.display_name, \
        a.bio, a.locked, a.bot, a.discoverable, a.avatar_path, \
        a.banner_path, a.avatar_remote_url, a.banner_remote_url, \
        a.avatar_blurhash, a.header_blurhash, \
        a.profile_fields, a.followers_count, a.following_count, \
        a.statuses_count, a.created_at, u.indexable, a.moved_to_account_id, a.suspended_at";

/// Load an `accounts` row by primary key. The `users` LEFT JOIN is how
/// `noindex` gets its value for local accounts; remote rows have no
/// `users` row and keep the historical `noindex: false`.
pub async fn load(pool: &sqlx::PgPool, account_id: i64) -> Result<Option<AccountRow>> {
    let row = sqlx::query(&format!(
        "SELECT {ACCOUNT_COLS} \
         FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
         WHERE a.id = $1 AND a.deleted_at IS NULL"
    ))
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| account_row(&r)))
}

/// Load a local `accounts` row by username (case-insensitive, matching
/// the actor-lookup rule). Suspended and deleted accounts resolve to
/// `None` — callers serving them would leak their tombstone state.
pub async fn load_local_by_username(
    pool: &sqlx::PgPool,
    username: &str,
) -> Result<Option<AccountRow>> {
    let row = sqlx::query(&format!(
        "SELECT {ACCOUNT_COLS} \
         FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
         WHERE lower(a.username) = $1 AND a.domain IS NULL \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL"
    ))
    .bind(username.to_lowercase())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| account_row(&r)))
}

/// Load many `accounts` rows in one query — the listing form of
/// [`load`]. Result order matches `ids`; missing/deleted/suspended
/// ids are dropped (the caller loses the position). Admin surfaces
/// needing suspended rows query `accounts` directly.
pub async fn load_many(pool: &sqlx::PgPool, ids: &[i64]) -> Result<Vec<AccountRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {ACCOUNT_COLS} \
         FROM accounts a LEFT JOIN users u ON u.account_id = a.id \
         WHERE a.id = ANY($1) AND a.deleted_at IS NULL AND a.suspended_at IS NULL \
         ORDER BY array_position($1::bigint[], a.id)"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(account_row).collect())
}

fn account_row(r: &sqlx::postgres::PgRow) -> AccountRow {
    AccountRow {
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
        moved_to_account_id: r.get("moved_to_account_id"),
        suspended_at: r.get("suspended_at"),
        created_at: r.get("created_at"),
    }
}

/// Public media URL for a stored key (`avatars/…`, `headers/…`) or the
/// remote URL — the remote value wins for remote rows (which never
/// carry a local key).
fn media_url(
    config: &crate::config::Config,
    base_url: &str,
    local_key: &Option<String>,
    remote: &Option<String>,
) -> String {
    let local = local_key
        .as_ref()
        .map(|k| crate::media::public_url(config, base_url, k));
    let remote = remote.clone();
    // With remote-media caching on, a locally cached copy wins; off,
    // the remote URL is served directly.
    if config.media.fetch_remote_media {
        local.or(remote).unwrap_or_default()
    } else {
        remote.or(local).unwrap_or_default()
    }
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
pub fn to_account(account: &AccountRow, base_url: &str, config: &crate::config::Config) -> Value {
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
        "avatar": media_url(config, base_url, &account.avatar_path, &account.avatar_remote_url),
        "avatar_static": media_url(config, base_url, &account.avatar_path, &account.avatar_remote_url),
        "header": media_url(config, base_url, &account.banner_path, &account.banner_remote_url),
        "header_static": media_url(config, base_url, &account.banner_path, &account.banner_remote_url),
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
        // CONTRACT: `moved` is a *placeholder* — `to_account` is
        // synchronous and cannot load the nested entity. Every caller
        // rendering an Account for a possibly-moved account MUST
        // overwrite this with `moves::moved_account_json` (see
        // `verify_credentials`); otherwise a moved account silently
        // renders `"moved": null` and clients won't redirect.
        "moved": Value::Null,
        "emojis": [],
        "yonder": {
            "avatar_blurhash": account.avatar_blurhash,
            "header_blurhash": account.header_blurhash,
        },
    })
}

/// The texts that can hold a `:code:` for one account — bio,
/// display name and each profile field's name/value. Deliberately
/// NOT the serialized `profile_fields` JSON: its `"name":`
/// punctuation would defeat [`EmojiMap::for_author_texts`]'s
/// short-circuit on every account.
pub(crate) fn emoji_texts(account: &AccountRow) -> Vec<&str> {
    let mut texts: Vec<&str> = vec![
        account.bio.as_deref().unwrap_or_default(),
        account.display_name.as_deref().unwrap_or_default(),
    ];
    if let Some(fields) = account.profile_fields.as_array() {
        for f in fields {
            for key in ["name", "value", "value_raw"] {
                if let Some(v) = f.get(key).and_then(Value::as_str) {
                    texts.push(v);
                }
            }
        }
    }
    texts
}

/// The full Account entity every API surface should emit:
/// `to_account` + the nested `moved` target + the epic-20 emoji
/// pass (`note`/`fields` `<img>` substitution and the `emojis`
/// array, domain-scoped). `to_account` alone leaves `:code:`
/// unrendered and `moved` unset — prefer this.
pub async fn account_entity(
    db: &sqlx::PgPool,
    config: &crate::config::Config,
    base_url: &str,
    account: &AccountRow,
) -> Result<Value> {
    Ok(
        account_entities(db, config, base_url, std::slice::from_ref(account))
            .await?
            .into_iter()
            .next()
            .expect("one entity per account"),
    )
}

/// `account_entity` for a page of accounts: the emoji map is loaded
/// once per distinct author domain (the common case is one local
/// map for the whole page) and only when some account's text could
/// hold a `:code:` — a 40-account list issues at most one
/// `custom_emojis` read instead of one per entity.
pub async fn account_entities(
    db: &sqlx::PgPool,
    config: &crate::config::Config,
    base_url: &str,
    accounts: &[AccountRow],
) -> Result<Vec<Value>> {
    use crate::emoji::render::{apply_account_with_map, has_shortcode_candidate, EmojiMap};
    let mut maps: std::collections::HashMap<Option<String>, EmojiMap> =
        std::collections::HashMap::new();
    let mut out = Vec::with_capacity(accounts.len());
    for account in accounts {
        let mut entity = to_account(account, base_url, config);
        entity["moved"] =
            crate::users::moves::moved_account_json(db, account, base_url, config).await;
        if emoji_texts(account)
            .iter()
            .any(|t| has_shortcode_candidate(t))
        {
            let key = account.domain.clone();
            if !maps.contains_key(&key) {
                maps.insert(
                    key.clone(),
                    EmojiMap::for_author(db, config, base_url, key.as_deref()).await?,
                );
            }
            apply_account_with_map(&mut entity, &maps[&key]);
        }
        out.push(entity);
    }
    Ok(out)
}
