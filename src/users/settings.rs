//! User settings (epic 10): persistence, validation and JSON
//! projections for the BOOTSTRAP "Users → User settings" surface.
//!
//! Three consumers read the same [`UserSettings`] row: the
//! CredentialAccount `source`/`yonder.settings` blocks, Mastodon
//! `GET /api/v1/preferences`, and the `GET`/`PATCH
//! /api/v1/yonder/settings` extension endpoint. Mastodon-native
//! settings additionally write through `PATCH
//! /api/v1/accounts/update_credentials` (`source[privacy]`,
//! `source[language]`, `source[sensitive]`, `hide_collections`,
//! `indexable`); everything else writes through the yonder setter.
//!
//! NULL is a meaningful state for `locale`, `theme`, `accent`,
//! `default_language` and `scheduled_deletion_days`: the column stays
//! NULL while the user has not chosen, and reads resolve the effective
//! value (system language via `Accept-Language`, `theme = system`,
//! `0` deletion days).

use std::collections::{HashMap, HashSet};

use axum::http::{header::ACCEPT_LANGUAGE, HeaderMap};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{config::Config, http::middleware::auth::AuthContext, Error, Result};

pub use crate::i18n::SUPPORTED_LOCALES;

/// Upper bound for `scheduled_deletion_days` (BOOTSTRAP gives none;
/// the epic-10 plan fixes 10 years). `0` means "never".
pub const MAX_SCHEDULED_DELETION_DAYS: i32 = 3650;

/// `filter_languages` array bound — a cap against abuse, not a UI limit.
pub const MAX_FILTER_LANGUAGES: usize = 100;

/// One BCP-47 tag's total length bound.
const MAX_LANGUAGE_TAG_LEN: usize = 35;

/// Default-post visibilities BOOTSTRAP allows. `direct` is not a valid
/// *default* even though the DB CHECK (from epic 02) admits the value.
pub const VISIBILITIES: &[&str] = &["public", "unlisted", "followers_only", "private"];

/// Stored `users.theme` values; NULL resolves to `system`.
pub const THEMES: &[&str] = &["light", "dark", "system"];

// ---------------------------------------------------------------------------
// Patch tri-state
// ---------------------------------------------------------------------------

/// A field in a partial settings update: absent (unchanged) vs.
/// explicit reset (JSON `null`, form `""`) vs. a new value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Patch<T> {
    /// Key not present in the request — leave the column alone.
    #[default]
    Unset,
    /// Explicit `null`/`""` — restore the BOOTSTRAP default (or NULL
    /// for nullable columns).
    Reset,
    /// A new value.
    Set(T),
}

impl<T> Patch<T> {
    pub fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }

    /// Unset keeps `current`, Reset restores `default`, Set wins.
    pub fn resolve(self, current: T, default: T) -> T {
        match self {
            Self::Unset => current,
            Self::Reset => default,
            Self::Set(v) => v,
        }
    }

    /// Nullable-column variant: Reset clears to NULL.
    pub fn resolve_nullable(self, current: Option<T>) -> Option<T> {
        match self {
            Self::Unset => current,
            Self::Reset => None,
            Self::Set(v) => Some(v),
        }
    }
}

/// `true`-ish form/JSON-string values (Mastodon sends strings for
/// booleans). An empty string is *not* a boolean — callers map it to a
/// reset themselves.
pub fn parse_bool_str(value: &str) -> Result<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(Error::Validation(format!(
            "invalid boolean value {value:?} — expected true/false"
        ))),
    }
}

/// Form-transport boolean: absent → Unset, `""` → Reset, truthy → Set.
pub fn form_flag_patch(pairs: &HashMap<String, String>, key: &str) -> Result<Patch<bool>> {
    match pairs.get(key) {
        None => Ok(Patch::Unset),
        Some(s) if s.trim().is_empty() => Ok(Patch::Reset),
        Some(s) => Ok(Patch::Set(parse_bool_str(s)?)),
    }
}

/// JSON boolean: absent → Unset, `null`/`""` → Reset, bool or
/// truthy-string → Set.
pub fn json_flag_patch(body: &Value, key: &str) -> Result<Patch<bool>> {
    match body.get(key) {
        None => Ok(Patch::Unset),
        Some(Value::Null) => Ok(Patch::Reset),
        Some(Value::Bool(b)) => Ok(Patch::Set(*b)),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Patch::Reset),
        Some(Value::String(s)) => Ok(Patch::Set(parse_bool_str(s)?)),
        _ => Err(Error::Validation(format!("{key} must be a boolean"))),
    }
}

/// Form-transport string: absent → Unset, `""` → Reset.
pub fn pair_string_patch(pairs: &HashMap<String, String>, key: &str) -> Patch<String> {
    match pairs.get(key) {
        None => Patch::Unset,
        Some(s) if s.trim().is_empty() => Patch::Reset,
        Some(s) => Patch::Set(s.trim().to_string()),
    }
}

/// JSON string: absent → Unset, `null`/`""` → Reset.
pub fn json_string_patch(body: &Value, key: &str) -> Result<Patch<String>> {
    match body.get(key) {
        None => Ok(Patch::Unset),
        Some(Value::Null) => Ok(Patch::Reset),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Patch::Reset),
        Some(Value::String(s)) => Ok(Patch::Set(s.clone())),
        _ => Err(Error::Validation(format!("{key} must be a string"))),
    }
}

/// Merge a nested `source.x` value with its flat `source_x` alias —
/// both present means a conflict, not a winner.
pub fn merge_alias_patch<T: PartialEq>(
    nested: Patch<T>,
    alias: Patch<T>,
    name: &str,
) -> Result<Patch<T>> {
    if !nested.is_unset() && !alias.is_unset() && nested != alias {
        return Err(Error::Validation(format!("conflicting values for {name}")));
    }
    Ok(if alias.is_unset() { nested } else { alias })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `locale`/`default_language`: one of the five supported codes
/// (normalized to lowercase first).
pub fn normalize_locale(code: &str) -> String {
    code.trim().to_ascii_lowercase()
}

pub fn validate_supported_locale(field: &str, code: &str) -> Result<()> {
    if SUPPORTED_LOCALES.contains(&code) {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "{field} must be one of: {}",
        SUPPORTED_LOCALES.join(", ")
    )))
}

pub fn validate_theme(theme: &str) -> Result<()> {
    if THEMES.contains(&theme) {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "theme must be one of: {}",
        THEMES.join(", ")
    )))
}

/// Accent color: normalized `#rrggbb` or none — never arbitrary CSS.
pub fn normalize_accent(accent: &str) -> Result<Option<String>> {
    let trimmed = accent.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let hex = trimmed
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| Error::Validation("accent must be a #rrggbb color".to_string()))?;
    Ok(Some(format!("#{}", hex.to_ascii_lowercase())))
}

pub fn validate_visibility(visibility: &str) -> Result<()> {
    if VISIBILITIES.contains(&visibility) {
        return Ok(());
    }
    Err(Error::Validation(format!(
        "visibility must be one of: {}",
        VISIBILITIES.join(", ")
    )))
}

pub fn validate_deletion_days(days: i64) -> Result<i32> {
    if !(0..=MAX_SCHEDULED_DELETION_DAYS as i64).contains(&days) {
        return Err(Error::Validation(format!(
            "scheduled_deletion_days must be between 0 and {MAX_SCHEDULED_DELETION_DAYS}"
        )));
    }
    Ok(days as i32)
}

/// Conservative BCP-47 shape: alpha primary subtag, alphanumeric
/// subtags. Full validation is unnecessary — the value feeds a filter
/// list, not a locale resolver.
fn is_valid_language_tag(tag: &str) -> bool {
    if tag.len() > MAX_LANGUAGE_TAG_LEN {
        return false;
    }
    let mut subtags = tag.split('-');
    let primary = match subtags.next() {
        Some(p) => p,
        None => return false,
    };
    if !(2..=8).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    subtags.all(|s| (1..=8).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Lowercase, dedupe and bound a submitted `filter_languages` list.
/// The list is not restricted to the UI locales — users filter
/// arbitrary post languages.
pub fn normalize_filter_languages(tags: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for tag in tags {
        let normalized = tag.trim().to_ascii_lowercase();
        if !is_valid_language_tag(&normalized) {
            return Err(Error::Validation(format!(
                "invalid language tag {tag:?} in filter_languages"
            )));
        }
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    if out.len() > MAX_FILTER_LANGUAGES {
        return Err(Error::Validation(format!(
            "at most {MAX_FILTER_LANGUAGES} filter languages are allowed"
        )));
    }
    Ok(out)
}

/// Stored `accounts.default_visibility` → the value reported on the
/// wire. Legacy `direct` rows (the DB CHECK admits it, the API never
/// writes it) render conservatively as `private` without rewriting
/// storage.
pub fn public_visibility(stored: &str) -> Value {
    match stored {
        "direct" => {
            tracing::warn!("legacy direct default_visibility rendered as private");
            Value::String("private".to_string())
        }
        other => Value::String(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Language resolution
// ---------------------------------------------------------------------------

/// `Accept-Language` request header as an owned string.
pub fn accept_language(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// `Accept-Language` → the best supported locale, honoring `q=`
/// weights and region fallback (`es-MX` → `es`). `None` when no entry
/// matches the supported set.
pub fn negotiate_language(header: Option<&str>) -> Option<String> {
    let header = header?;
    let mut candidates: Vec<(f32, String)> = Vec::new();
    for entry in header.split(',') {
        let mut parts = entry.trim().split(';');
        let tag = parts.next().unwrap_or("").trim();
        if tag.is_empty() || tag == "*" {
            continue;
        }
        let q = parts
            .filter_map(|p| p.trim().strip_prefix("q="))
            .find_map(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0);
        if q <= 0.0 {
            continue;
        }
        candidates.push((q, tag.to_ascii_lowercase()));
    }
    // Highest q wins; input order breaks ties (a client's list is
    // already most-preferred-first).
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, tag) in candidates {
        if SUPPORTED_LOCALES.contains(&tag.as_str()) {
            return Some(tag);
        }
        if let Some((primary, _)) = tag.split_once('-') {
            if SUPPORTED_LOCALES.contains(&primary) {
                return Some(primary.to_string());
            }
        }
    }
    None
}

/// Resolved effective language: stored posting language → stored
/// interface language → `Accept-Language` → `i18n.fallback_locale`.
pub fn effective_language(
    stored_posting: Option<&str>,
    stored_locale: Option<&str>,
    accept_language: Option<&str>,
    config: &Config,
) -> String {
    [stored_posting, stored_locale]
        .into_iter()
        .flatten()
        .find(|code| SUPPORTED_LOCALES.contains(code))
        .map(str::to_owned)
        .or_else(|| negotiate_language(accept_language))
        .unwrap_or_else(|| config.i18n.fallback_locale.clone())
}

// ---------------------------------------------------------------------------
// Stored row + projections
// ---------------------------------------------------------------------------

/// The epic-10 settings, joined across `users` and `accounts`.
#[derive(Debug, Clone)]
pub struct UserSettings {
    // accounts
    pub hide_network: bool,
    pub directory_opt_in: bool,
    pub default_visibility: String,
    pub default_language: Option<String>,
    pub default_sensitive: bool,
    // users
    pub indexable: bool,
    pub include_public_posts_in_search: bool,
    pub show_app_display: bool,
    pub cw_always_expand: bool,
    pub block_stranger_notifications: bool,
    /// NULL in storage resolves to `0` (never).
    pub scheduled_deletion_days: i32,
    pub theme: Option<String>,
    pub accent: Option<String>,
    pub locale: Option<String>,
    pub filter_languages: Vec<String>,
    pub email_notif_follow: bool,
    pub email_notif_follow_request: bool,
    pub email_notif_reblog: bool,
    pub email_notif_favourite: bool,
    pub email_notif_mention: bool,
    pub email_notif_quote: bool,
}

/// Load the joined settings row for a user.
pub async fn load(pool: &PgPool, user_id: i64) -> Result<UserSettings> {
    let row = sqlx::query(
        "SELECT a.hide_network, a.directory_opt_in, a.default_visibility, \
                a.default_language, a.default_sensitive, \
                u.indexable, u.include_public_posts_in_search, \
                u.show_app_display, u.cw_always_expand, \
                u.block_stranger_notifications, u.scheduled_deletion_days, \
                u.theme, u.accent, u.locale, u.filter_languages, \
                u.email_notif_follow, u.email_notif_follow_request, \
                u.email_notif_reblog, u.email_notif_favourite, \
                u.email_notif_mention, u.email_notif_quote \
         FROM users u JOIN accounts a ON a.id = u.account_id \
         WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound("user".into()))?;
    Ok(UserSettings {
        hide_network: row.get("hide_network"),
        directory_opt_in: row.get("directory_opt_in"),
        default_visibility: row.get("default_visibility"),
        default_language: row.get("default_language"),
        default_sensitive: row.get("default_sensitive"),
        indexable: row.get("indexable"),
        include_public_posts_in_search: row.get("include_public_posts_in_search"),
        show_app_display: row.get("show_app_display"),
        cw_always_expand: row.get("cw_always_expand"),
        block_stranger_notifications: row.get("block_stranger_notifications"),
        scheduled_deletion_days: row
            .get::<Option<i32>, _>("scheduled_deletion_days")
            .unwrap_or(0),
        theme: row.get("theme"),
        accent: row.get("accent"),
        locale: row.get("locale"),
        filter_languages: row.get("filter_languages"),
        email_notif_follow: row.get("email_notif_follow"),
        email_notif_follow_request: row.get("email_notif_follow_request"),
        email_notif_reblog: row.get("email_notif_reblog"),
        email_notif_favourite: row.get("email_notif_favourite"),
        email_notif_mention: row.get("email_notif_mention"),
        email_notif_quote: row.get("email_notif_quote"),
    })
}

fn email_notifications_json(s: &UserSettings) -> Value {
    json!({
        "follow": s.email_notif_follow,
        "follow_request": s.email_notif_follow_request,
        "reblog": s.email_notif_reblog,
        "favourite": s.email_notif_favourite,
        "mention": s.email_notif_mention,
        "quote": s.email_notif_quote,
    })
}

impl UserSettings {
    /// The canonical `yonder.settings` object — identical in the
    /// CredentialAccount `yonder` block, `GET /api/v1/yonder/settings`
    /// and `GET /api/v1/preferences`.`yonder`. Nullable fields report
    /// the stored value (`null` = unset/follow-system), and
    /// `effective_posting_language` gives the resolved default for
    /// composer UIs.
    pub fn yonder_json(&self, config: &Config, accept_language: Option<&str>) -> Value {
        json!({
            "interface_language": self.locale,
            "default_posting_language": self.default_language,
            "default_visibility": public_visibility(&self.default_visibility),
            "default_sensitive": self.default_sensitive,
            "theme": self.theme.as_deref().unwrap_or("system"),
            "accent": self.accent,
            "block_stranger_notifications": self.block_stranger_notifications,
            "scheduled_deletion_days": self.scheduled_deletion_days,
            "show_app_display": self.show_app_display,
            "include_public_posts_in_search": self.include_public_posts_in_search,
            "directory_opt_in": self.directory_opt_in,
            "cw_always_expand": self.cw_always_expand,
            "filter_languages": self.filter_languages,
            "email_notifications": email_notifications_json(self),
            "supported_locales": SUPPORTED_LOCALES,
            "effective_posting_language": effective_language(
                self.default_language.as_deref(),
                self.locale.as_deref(),
                accept_language,
                config,
            ),
        })
    }

    /// Mastodon `Preferences` entity — native keys at top level,
    /// nonstandard settings under `yonder`.
    pub fn preferences_json(&self, config: &Config, accept_language: Option<&str>) -> Value {
        json!({
            "posting:default:visibility": public_visibility(&self.default_visibility),
            "posting:default:sensitive": self.default_sensitive,
            "posting:default:language": effective_language(
                self.default_language.as_deref(),
                self.locale.as_deref(),
                accept_language,
                config,
            ),
            "reading:expand:media": "default",
            "reading:expand:spoilers": self.cw_always_expand,
            "yonder": self.yonder_json(config, accept_language),
        })
    }
}

// ---------------------------------------------------------------------------
// PATCH /api/v1/yonder/settings
// ---------------------------------------------------------------------------

/// Per-field patches for `email_notifications` — partial objects merge.
#[derive(Debug, Default)]
pub struct EmailNotifPatch {
    pub follow: Patch<bool>,
    pub follow_request: Patch<bool>,
    pub reblog: Patch<bool>,
    pub favourite: Patch<bool>,
    pub mention: Patch<bool>,
    pub quote: Patch<bool>,
}

/// A validated, partial update to [`UserSettings`]. Every field GET
/// returns is PATCH-able, so a read-modify-write round trip is safe.
#[derive(Debug, Default)]
pub struct SettingsPatch {
    pub interface_language: Patch<String>,
    pub default_posting_language: Patch<String>,
    pub default_visibility: Patch<String>,
    pub default_sensitive: Patch<bool>,
    pub theme: Patch<String>,
    pub accent: Patch<String>,
    pub block_stranger_notifications: Patch<bool>,
    pub scheduled_deletion_days: Patch<i32>,
    pub show_app_display: Patch<bool>,
    pub include_public_posts_in_search: Patch<bool>,
    pub directory_opt_in: Patch<bool>,
    pub cw_always_expand: Patch<bool>,
    pub filter_languages: Patch<Vec<String>>,
    pub email_notifications: Option<EmailNotifPatch>,
}

fn patch_bool(body: &Value, key: &str) -> Result<Patch<bool>> {
    match body.get(key) {
        None => Ok(Patch::Unset),
        Some(Value::Null) => Ok(Patch::Reset),
        Some(Value::Bool(b)) => Ok(Patch::Set(*b)),
        _ => Err(Error::Validation(format!(
            "{key} must be a boolean or null"
        ))),
    }
}

fn patch_i32(body: &Value, key: &str) -> Result<Patch<i32>> {
    match body.get(key) {
        None => Ok(Patch::Unset),
        Some(Value::Null) => Ok(Patch::Reset),
        Some(Value::Number(n)) => {
            let days = n
                .as_i64()
                .ok_or_else(|| Error::Validation(format!("{key} must be an integer")))?;
            Ok(Patch::Set(validate_deletion_days(days)?))
        }
        _ => Err(Error::Validation(format!(
            "{key} must be an integer or null"
        ))),
    }
}

fn patch_string_vec(body: &Value, key: &str) -> Result<Patch<Vec<String>>> {
    match body.get(key) {
        None => Ok(Patch::Unset),
        Some(Value::Null) => Ok(Patch::Reset),
        Some(Value::Array(items)) => {
            let tags: Vec<String> = items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| Error::Validation(format!("{key} entries must be strings")))
                })
                .collect::<Result<_>>()?;
            Ok(Patch::Set(normalize_filter_languages(tags)?))
        }
        _ => Err(Error::Validation(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn parse_email_notif(value: Option<&Value>) -> Result<Option<EmailNotifPatch>> {
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(EmailNotifPatch {
            follow: Patch::Reset,
            follow_request: Patch::Reset,
            reblog: Patch::Reset,
            favourite: Patch::Reset,
            mention: Patch::Reset,
            quote: Patch::Reset,
        })),
        Some(Value::Object(map)) => {
            let obj = Value::Object(map.clone());
            for key in map.keys() {
                match key.as_str() {
                    "follow" | "follow_request" | "reblog" | "favourite" | "mention" | "quote" => {}
                    _ => {
                        return Err(Error::Validation(format!(
                            "unknown email_notifications key {key:?}"
                        )))
                    }
                }
            }
            Ok(Some(EmailNotifPatch {
                follow: patch_bool(&obj, "follow")?,
                follow_request: patch_bool(&obj, "follow_request")?,
                reblog: patch_bool(&obj, "reblog")?,
                favourite: patch_bool(&obj, "favourite")?,
                mention: patch_bool(&obj, "mention")?,
                quote: patch_bool(&obj, "quote")?,
            }))
        }
        Some(_) => Err(Error::Validation(
            "email_notifications must be an object".to_string(),
        )),
    }
}

/// Parse + validate `PATCH /api/v1/yonder/settings` input. Unknown
/// keys, wrong types and out-of-range values are all 422.
pub fn parse_patch(body: &Value) -> Result<SettingsPatch> {
    let obj = body
        .as_object()
        .ok_or_else(|| Error::Validation("settings must be a JSON object".to_string()))?;
    for key in obj.keys() {
        match key.as_str() {
            "interface_language"
            | "default_posting_language"
            | "default_visibility"
            | "default_sensitive"
            | "theme"
            | "accent"
            | "block_stranger_notifications"
            | "scheduled_deletion_days"
            | "show_app_display"
            | "include_public_posts_in_search"
            | "directory_opt_in"
            | "cw_always_expand"
            | "filter_languages"
            | "email_notifications" => {}
            _ => return Err(Error::Validation(format!("unknown settings key {key:?}"))),
        }
    }

    let interface_language = match json_string_patch(body, "interface_language")? {
        Patch::Set(code) => {
            let code = normalize_locale(&code);
            validate_supported_locale("interface_language", &code)?;
            Patch::Set(code)
        }
        other => other,
    };
    let default_posting_language = match json_string_patch(body, "default_posting_language")? {
        Patch::Set(code) => {
            let code = normalize_locale(&code);
            validate_supported_locale("default_posting_language", &code)?;
            Patch::Set(code)
        }
        other => other,
    };
    let default_visibility = match json_string_patch(body, "default_visibility")? {
        Patch::Set(v) => {
            let v = v.to_ascii_lowercase();
            validate_visibility(&v)?;
            Patch::Set(v)
        }
        other => other,
    };
    let theme = match json_string_patch(body, "theme")? {
        Patch::Set(t) => {
            let t = t.to_ascii_lowercase();
            validate_theme(&t)?;
            Patch::Set(t)
        }
        other => other,
    };
    let accent = match json_string_patch(body, "accent")? {
        Patch::Set(a) => match normalize_accent(&a)? {
            Some(a) => Patch::Set(a),
            None => Patch::Reset,
        },
        other => other,
    };

    Ok(SettingsPatch {
        interface_language,
        default_posting_language,
        default_visibility,
        default_sensitive: patch_bool(body, "default_sensitive")?,
        theme,
        accent,
        block_stranger_notifications: patch_bool(body, "block_stranger_notifications")?,
        scheduled_deletion_days: patch_i32(body, "scheduled_deletion_days")?,
        show_app_display: patch_bool(body, "show_app_display")?,
        include_public_posts_in_search: patch_bool(body, "include_public_posts_in_search")?,
        directory_opt_in: patch_bool(body, "directory_opt_in")?,
        cw_always_expand: patch_bool(body, "cw_always_expand")?,
        filter_languages: patch_string_vec(body, "filter_languages")?,
        email_notifications: parse_email_notif(body.get("email_notifications"))?,
    })
}

/// Apply a validated patch under row locks — `accounts` before
/// `users`, the same order `users::profile::update_profile` uses.
pub async fn save(db: &PgPool, auth: &AuthContext, patch: &SettingsPatch) -> Result<()> {
    let mut tx = db.begin().await?;

    let acct = sqlx::query(
        "SELECT directory_opt_in, default_visibility, default_language, \
                default_sensitive \
         FROM accounts WHERE id = $1 AND domain IS NULL FOR UPDATE",
    )
    .bind(auth.account_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;

    let user = sqlx::query(
        "SELECT indexable, include_public_posts_in_search, show_app_display, \
                cw_always_expand, block_stranger_notifications, \
                scheduled_deletion_days, theme, accent, locale, \
                filter_languages, email_notif_follow, \
                email_notif_follow_request, email_notif_reblog, \
                email_notif_favourite, email_notif_mention, email_notif_quote \
         FROM users WHERE id = $1 FOR UPDATE",
    )
    .bind(auth.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::NotFound("user".into()))?;

    // accounts columns — only UPDATE when the patch touches them.
    let accounts_touched = !patch.directory_opt_in.is_unset()
        || !patch.default_visibility.is_unset()
        || !patch.default_posting_language.is_unset()
        || !patch.default_sensitive.is_unset();
    if accounts_touched {
        sqlx::query(
            "UPDATE accounts SET directory_opt_in = $2, default_visibility = $3, \
                    default_language = $4, default_sensitive = $5, \
                    updated_at = now() WHERE id = $1",
        )
        .bind(auth.account_id)
        .bind(
            patch
                .directory_opt_in
                .clone()
                .resolve(acct.get("directory_opt_in"), true),
        )
        .bind(
            patch
                .default_visibility
                .clone()
                .resolve(acct.get("default_visibility"), "public".to_string()),
        )
        .bind(
            patch
                .default_posting_language
                .clone()
                .resolve_nullable(acct.get("default_language")),
        )
        .bind(
            patch
                .default_sensitive
                .clone()
                .resolve(acct.get("default_sensitive"), false),
        )
        .execute(&mut *tx)
        .await?;
    }

    let email = patch.email_notifications.as_ref();
    let users_touched = !patch.interface_language.is_unset()
        || !patch.theme.is_unset()
        || !patch.accent.is_unset()
        || !patch.block_stranger_notifications.is_unset()
        || !patch.scheduled_deletion_days.is_unset()
        || !patch.show_app_display.is_unset()
        || !patch.include_public_posts_in_search.is_unset()
        || !patch.cw_always_expand.is_unset()
        || !patch.filter_languages.is_unset()
        || email.is_some();
    if users_touched {
        let email_patch =
            |current: bool, get: fn(&EmailNotifPatch) -> &Patch<bool>, default: bool| {
                email.map_or(current, |e| get(e).clone().resolve(current, default))
            };
        sqlx::query(
            "UPDATE users SET locale = $2, theme = $3, accent = $4, \
                    block_stranger_notifications = $5, \
                    scheduled_deletion_days = $6, show_app_display = $7, \
                    include_public_posts_in_search = $8, \
                    cw_always_expand = $9, filter_languages = $10, \
                    email_notif_follow = $11, email_notif_follow_request = $12, \
                    email_notif_reblog = $13, email_notif_favourite = $14, \
                    email_notif_mention = $15, email_notif_quote = $16, \
                    updated_at = now() WHERE id = $1",
        )
        .bind(auth.user_id)
        .bind(
            patch
                .interface_language
                .clone()
                .resolve_nullable(user.get("locale")),
        )
        .bind(patch.theme.clone().resolve_nullable(user.get("theme")))
        .bind(patch.accent.clone().resolve_nullable(user.get("accent")))
        .bind(
            patch
                .block_stranger_notifications
                .clone()
                .resolve(user.get("block_stranger_notifications"), false),
        )
        .bind(
            patch
                .scheduled_deletion_days
                .clone()
                .resolve_nullable(user.get("scheduled_deletion_days")),
        )
        .bind(
            patch
                .show_app_display
                .clone()
                .resolve(user.get("show_app_display"), true),
        )
        .bind(
            patch
                .include_public_posts_in_search
                .clone()
                .resolve(user.get("include_public_posts_in_search"), true),
        )
        .bind(
            patch
                .cw_always_expand
                .clone()
                .resolve(user.get("cw_always_expand"), false),
        )
        .bind(
            patch
                .filter_languages
                .clone()
                .resolve(user.get("filter_languages"), Vec::new()),
        )
        .bind(email_patch(
            user.get("email_notif_follow"),
            |e| &e.follow,
            false,
        ))
        .bind(email_patch(
            user.get("email_notif_follow_request"),
            |e| &e.follow_request,
            true,
        ))
        .bind(email_patch(
            user.get("email_notif_reblog"),
            |e| &e.reblog,
            false,
        ))
        .bind(email_patch(
            user.get("email_notif_favourite"),
            |e| &e.favourite,
            false,
        ))
        .bind(email_patch(
            user.get("email_notif_mention"),
            |e| &e.mention,
            true,
        ))
        .bind(email_patch(
            user.get("email_notif_quote"),
            |e| &e.quote,
            true,
        ))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_language_picks_best_supported() {
        assert_eq!(negotiate_language(Some("it")), Some("it".to_string()));
        assert_eq!(
            negotiate_language(Some("fr-CA,fr;q=0.9,es-MX;q=0.8")),
            Some("es".to_string())
        );
        assert_eq!(
            negotiate_language(Some("fr;q=0.9,de-AT;q=0.95")),
            Some("de".to_string())
        );
        assert_eq!(negotiate_language(Some("fr,ja,*;q=0.5")), None);
        assert_eq!(negotiate_language(Some("es;q=0")), None);
        assert_eq!(negotiate_language(None), None);
    }

    #[test]
    fn filter_languages_normalizes_and_dedupes() {
        let out = normalize_filter_languages(vec![
            "IT".to_string(),
            "de-AT".to_string(),
            "it".to_string(),
        ])
        .unwrap();
        assert_eq!(out, vec!["it", "de-at"]);
        assert!(normalize_filter_languages(vec!["x".to_string()]).is_err());
        assert!(normalize_filter_languages(vec!["not a tag!".to_string()]).is_err());
    }

    #[test]
    fn accent_validation() {
        assert_eq!(
            normalize_accent("#A1B2C3").unwrap(),
            Some("#a1b2c3".to_string())
        );
        assert_eq!(normalize_accent("  ").unwrap(), None);
        assert!(normalize_accent("red").is_err());
        assert!(normalize_accent("#abc").is_err());
    }

    #[test]
    fn patch_round_trip_semantics() {
        assert_eq!(Patch::Set(3).resolve(1, 0), 3);
        assert_eq!(Patch::<i32>::Reset.resolve(1, 0), 0);
        assert_eq!(Patch::<i32>::Unset.resolve(1, 0), 1);
        assert_eq!(
            Patch::Set("x".to_string()).resolve_nullable(None),
            Some("x".to_string())
        );
        assert_eq!(
            Patch::<String>::Reset.resolve_nullable(Some("x".to_string())),
            None
        );
    }

    #[test]
    fn parse_patch_strict_and_validated() {
        assert!(parse_patch(&json!({"bogus": 1})).is_err());
        assert!(parse_patch(&json!({"theme": "sepia"})).is_err());
        assert!(parse_patch(&json!({"default_visibility": "direct"})).is_err());
        assert!(parse_patch(&json!({"scheduled_deletion_days": 99999})).is_err());
        assert!(parse_patch(&json!({"interface_language": "fr"})).is_err());
        assert!(parse_patch(&json!({"email_notifications": {"zzz": true}})).is_err());
        let patch = parse_patch(&json!({
            "theme": "dark",
            "interface_language": null,
            "scheduled_deletion_days": 30,
            "email_notifications": {"follow": true},
            "filter_languages": ["IT", " de-AT "],
        }))
        .unwrap();
        assert_eq!(patch.theme, Patch::Set("dark".to_string()));
        assert_eq!(patch.interface_language, Patch::Reset);
        assert_eq!(patch.scheduled_deletion_days, Patch::Set(30));
        assert_eq!(
            patch.filter_languages,
            Patch::Set(vec!["it".to_string(), "de-at".to_string()])
        );
        assert_eq!(patch.email_notifications.unwrap().follow, Patch::Set(true));
    }
}
