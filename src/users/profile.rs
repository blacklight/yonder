//! Profile editing (epic 09): `PATCH /api/v1/accounts/update_credentials`
//! parameter handling, validation, profile-field rendering,
//! `rel="me"` verification and the `VerifyProfileLinks` job.
//!
//! Data model: `accounts.profile_fields` is a JSONB array of
//! `{name, value, value_raw, verified_at}` — `value` is pre-rendered
//! HTML (a `<a rel="me">` anchor for URLs, escaped/linkified text
//! otherwise, already the AP wire shape), `value_raw` feeds
//! `source.fields` and the verifier, `verified_at` is stamped by
//! [`VerifyProfileLinksJob`]. `accounts.bio` holds **raw** text for
//! local accounts and is rendered on read.
//!
//! PATCH performs no network I/O: verification is enqueued on edit and
//! periodically re-run by the cron sweep — see [`VerifyProfileLinksJob`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Form, FromRequest, Json, Multipart, Request},
    http::header,
};
use futures_util::StreamExt;
use pubby_sdk::{http::HttpClient, is_linkable_url, render_bio_html, render_verified_link};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    config::Config,
    http::{middleware::auth::AuthContext, AppState},
    jobs::{JobContext, JobHandler, JobKind},
    media::{self, MediaKind},
    users::settings::{self, Patch},
    Error, Result,
};

/// One `fields_attributes[i]` entry as submitted.
#[derive(Debug, Clone)]
pub struct FieldInput {
    /// Field name (trimmed before storage).
    pub name: String,
    /// Field value — raw text or URL.
    pub value: String,
}

/// What a PATCH asks for an avatar/header slot.
#[derive(Debug, Clone, Default)]
pub enum MediaOp {
    /// Parameter absent — column untouched.
    #[default]
    Unchanged,
    /// Parameter present but empty — clear the column (Mastodon
    /// removal semantics).
    Clear,
    /// Uploaded bytes to validate, process and store.
    Set(Vec<u8>),
}

/// OpenAPI shape of the JSON form of `update_credentials` (files are
/// excluded — they only arrive via multipart).
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateCredentialsSchema {
    /// Display name (empty string clears).
    pub display_name: Option<String>,
    /// Raw bio text.
    pub note: Option<String>,
    /// Ordered profile fields.
    pub fields_attributes: Option<Vec<FieldInputSchema>>,
    /// Require follower approval.
    pub locked: Option<bool>,
    /// Bot account flag.
    pub bot: Option<bool>,
    /// Directory listing flag.
    pub discoverable: Option<bool>,
    /// Hide followers/following collections.
    pub hide_collections: Option<bool>,
    /// Opt out of search-engine indexing.
    pub indexable: Option<bool>,
    /// Posting defaults (`source[privacy]` etc. in form bodies).
    pub source: Option<SourceSchema>,
}

/// OpenAPI shape of the `source` posting-defaults block.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SourceSchema {
    /// Default post visibility
    /// (`public`/`unlisted`/`followers_only`/`private`).
    pub privacy: Option<String>,
    /// Mark new statuses sensitive by default.
    pub sensitive: Option<bool>,
    /// Default posting language (`en`/`es`/`it`/`de`/`ar`).
    pub language: Option<String>,
}

/// OpenAPI shape of one profile field.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FieldInputSchema {
    /// Field name.
    pub name: String,
    /// Field value (text or URL).
    pub value: String,
}

/// Normalized `update_credentials` parameters — the three transports
/// (multipart, urlencoded, JSON) all land here.
#[derive(Debug, Default)]
pub struct UpdateCredentials {
    /// `display_name` — `Some("")` clears to NULL.
    pub display_name: Option<String>,
    /// `note` — raw bio text.
    pub note: Option<String>,
    /// Avatar upload op.
    pub avatar: MediaOp,
    /// Header/banner upload op.
    pub header: MediaOp,
    /// `fields_attributes` — `Some` replaces the whole list.
    pub fields: Option<Vec<FieldInput>>,
    /// `locked` flag (epic-10 `Patch`: `""`/`null` resets to false).
    pub locked: Patch<bool>,
    /// `bot` flag.
    pub bot: Patch<bool>,
    /// `discoverable` flag.
    pub discoverable: Patch<bool>,
    /// `hide_collections` → `accounts.hide_network`.
    pub hide_collections: Patch<bool>,
    /// `indexable` → `users.indexable`.
    pub indexable: Patch<bool>,
    /// `source[sensitive]` → `accounts.default_sensitive`.
    pub sensitive: Patch<bool>,
    /// `source[privacy]` → `accounts.default_visibility`.
    pub privacy: Patch<String>,
    /// `source[language]` → `accounts.default_language`.
    pub language: Patch<String>,
}

// ---------------------------------------------------------------------------
// Transport parsing
// ---------------------------------------------------------------------------

/// Text-part cap — scalars and field values never legitimately exceed
/// this in one request.
const TEXT_PART_CAP: usize = 64 * 1024;

/// Parse the request body per its content type into normalized params.
/// Multipart carries files; urlencoded and JSON do not (`avatar`/
/// `header` in JSON may only be `""`/`null` → clear).
pub async fn parse_request(state: &Arc<AppState>, req: Request) -> Result<UpdateCredentials> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, state.as_ref())
            .await
            .map_err(|e| Error::Validation(format!("malformed multipart body: {e}")))?;
        parse_multipart(state, &mut mp).await
    } else if content_type.starts_with("application/x-www-form-urlencoded") {
        let Form(pairs) = Form::<HashMap<String, String>>::from_request(req, state.as_ref())
            .await
            .map_err(|e| Error::Validation(format!("malformed form body: {e}")))?;
        params_from_pairs(&pairs, MediaOp::Unchanged, MediaOp::Unchanged)
    } else {
        let Json(body) = Json::<Value>::from_request(req, state.as_ref())
            .await
            .map_err(|e| Error::Validation(format!("malformed JSON body: {e}")))?;
        params_from_json(&body)
    }
}

/// Read one multipart field to bytes with a hard cap — over-cap is a
/// 422 (uploads must not be silently truncated).
async fn read_capped(
    field: &mut axum::extract::multipart::Field<'_>,
    cap: u64,
    label: &str,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| Error::Validation(format!("failed to read {label}: {e}")))?
    {
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(Error::Validation(format!(
                "{label} is too large (at most {cap} bytes)"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Read a text part, truncating at `cap` and draining the rest — one
/// oversized scalar must not void the other parts (a valid upload in
/// the same body) the way a hard error would.
async fn read_text_part(
    field: &mut axum::extract::multipart::Field<'_>,
    cap: usize,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| Error::Validation(format!("failed to read text part: {e}")))?
    {
        let take = cap.saturating_sub(buf.len()).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    Ok(buf)
}

async fn parse_multipart(state: &Arc<AppState>, mp: &mut Multipart) -> Result<UpdateCredentials> {
    let max = state.config().media.max_avatar_size;
    let mut pairs: HashMap<String, String> = HashMap::new();
    let mut avatar = MediaOp::Unchanged;
    let mut header = MediaOp::Unchanged;
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|e| Error::Validation(format!("malformed multipart body: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "avatar" | "header" => {
                let has_filename = field.file_name().map(|f| !f.is_empty()).unwrap_or(false);
                let kind = if name == "avatar" {
                    MediaKind::Avatar
                } else {
                    MediaKind::Header
                };
                let bytes = read_capped(&mut field, max, kind.label()).await?;
                // An empty part (no filename, no bytes) is the Mastodon
                // "remove image" signal.
                let op = if !has_filename && bytes.is_empty() {
                    MediaOp::Clear
                } else {
                    MediaOp::Set(bytes)
                };
                if name == "avatar" {
                    avatar = op;
                } else {
                    header = op;
                }
            }
            _ => {
                let bytes = read_text_part(&mut field, TEXT_PART_CAP).await?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                pairs.insert(name, text);
            }
        }
    }
    params_from_pairs(&pairs, avatar, header)
}

/// Allocation guard for submitted field entries/indices — the real
/// count limit applies to non-empty entries later (`render_fields`),
/// so a sparse `[10]` slot carrying only blanks doesn't 422 a
/// 10-field edit.
const FIELD_ENTRY_HARD_CAP: usize = 1000;

/// `fields_attributes[0][name]`-style bracket keys → ordered entries.
/// Indices are ordering hints, not count slots: the number of
/// *non-empty* entries is what `max_profile_fields` limits.
fn parse_fields_attrs(pairs: &HashMap<String, String>) -> Result<Option<Vec<FieldInput>>> {
    let mut by_index: BTreeMap<usize, (String, String)> = BTreeMap::new();
    for (key, value) in pairs {
        let Some(rest) = key.strip_prefix("fields_attributes[") else {
            continue;
        };
        let Some((index, rest)) = rest.split_once(']') else {
            return Err(Error::Validation(format!(
                "malformed fields_attributes key {key:?}"
            )));
        };
        let index: usize = index
            .parse()
            .map_err(|_| Error::Validation(format!("malformed fields_attributes key {key:?}")))?;
        if index >= FIELD_ENTRY_HARD_CAP {
            return Err(Error::Validation(format!(
                "fields_attributes index {index} is too large"
            )));
        }
        let sub = rest
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or("");
        let entry = by_index.entry(index).or_default();
        match sub {
            "name" => entry.0 = value.clone(),
            "value" => entry.1 = value.clone(),
            _ => {
                return Err(Error::Validation(format!(
                    "unknown fields_attributes key {key:?}"
                )))
            }
        }
    }
    if by_index.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        by_index
            .into_values()
            .map(|(name, value)| FieldInput { name, value })
            .collect(),
    ))
}

fn params_from_pairs(
    pairs: &HashMap<String, String>,
    avatar: MediaOp,
    header: MediaOp,
) -> Result<UpdateCredentials> {
    Ok(UpdateCredentials {
        display_name: pairs.get("display_name").cloned(),
        note: pairs.get("note").cloned(),
        avatar,
        header,
        fields: parse_fields_attrs(pairs)?,
        locked: settings::form_flag_patch(pairs, "locked")?,
        bot: settings::form_flag_patch(pairs, "bot")?,
        discoverable: settings::form_flag_patch(pairs, "discoverable")?,
        hide_collections: settings::form_flag_patch(pairs, "hide_collections")?,
        indexable: settings::form_flag_patch(pairs, "indexable")?,
        sensitive: settings::form_flag_patch(pairs, "source[sensitive]")?,
        privacy: settings::pair_string_patch(pairs, "source[privacy]"),
        language: settings::pair_string_patch(pairs, "source[language]"),
    })
}

fn json_media_op(body: &Value, key: &str) -> Result<MediaOp> {
    match body.get(key) {
        None => Ok(MediaOp::Unchanged),
        Some(Value::Null) => Ok(MediaOp::Clear),
        Some(Value::String(s)) if s.is_empty() => Ok(MediaOp::Clear),
        Some(_) => Err(Error::Validation(format!(
            "{key} must be uploaded as multipart/form-data"
        ))),
    }
}

fn json_string(body: &Value, key: &str) -> Result<Option<String>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Error::Validation(format!("{key} must be a string"))),
    }
}

fn params_from_json(body: &Value) -> Result<UpdateCredentials> {
    let fields = match body.get("fields_attributes") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| FieldInput {
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    value: item
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect(),
        ),
        // Mastodon JSON clients also send `{"0": {"name": …}, …}` maps.
        Some(Value::Object(map)) => {
            let mut items: Vec<(&String, &Value)> = map.iter().collect();
            items.sort_by_key(|(k, _)| k.parse::<usize>().unwrap_or(usize::MAX));
            Some(
                items
                    .into_iter()
                    .map(|(_, item)| FieldInput {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        value: item
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect::<Vec<FieldInput>>(),
            )
        }
        Some(_) => {
            return Err(Error::Validation(
                "fields_attributes must be an array of {name, value}".into(),
            ))
        }
    };
    if let Some(fields) = &fields {
        // Coarse allocation guard; the real `max_profile_fields` check
        // counts non-empty entries in `render_fields`.
        if fields.len() > FIELD_ENTRY_HARD_CAP {
            return Err(Error::Validation(format!(
                "at most {FIELD_ENTRY_HARD_CAP} fields_attributes entries are allowed"
            )));
        }
    }

    // `source: {privacy, sensitive, language}` plus flat `source_*`
    // aliases; a nested/flat disagreement is a 422, never a coin flip.
    let source = match body.get("source") {
        None | Some(Value::Null) => None,
        Some(map @ Value::Object(_)) => Some(map.clone()),
        Some(_) => {
            return Err(Error::Validation(
                "source must be an object of {privacy, sensitive, language}".into(),
            ))
        }
    };
    if let Some(map) = source.as_ref().and_then(Value::as_object) {
        for key in map.keys() {
            match key.as_str() {
                "privacy" | "sensitive" | "language" => {}
                _ => {
                    return Err(Error::Validation(format!("unknown source key {key:?}")));
                }
            }
        }
    }
    let source_value = source.unwrap_or(Value::Null);
    let privacy = settings::merge_alias_patch(
        settings::json_string_patch(&source_value, "privacy")?,
        settings::json_string_patch(body, "source_privacy")?,
        "source[privacy]",
    )?;
    let language = settings::merge_alias_patch(
        settings::json_string_patch(&source_value, "language")?,
        settings::json_string_patch(body, "source_language")?,
        "source[language]",
    )?;
    let sensitive = settings::merge_alias_patch(
        settings::json_flag_patch(&source_value, "sensitive")?,
        settings::json_flag_patch(body, "source_sensitive")?,
        "source[sensitive]",
    )?;

    Ok(UpdateCredentials {
        display_name: json_string(body, "display_name")?,
        note: json_string(body, "note")?,
        avatar: json_media_op(body, "avatar")?,
        header: json_media_op(body, "header")?,
        fields,
        locked: settings::json_flag_patch(body, "locked")?,
        bot: settings::json_flag_patch(body, "bot")?,
        discoverable: settings::json_flag_patch(body, "discoverable")?,
        hide_collections: settings::json_flag_patch(body, "hide_collections")?,
        indexable: settings::json_flag_patch(body, "indexable")?,
        sensitive,
        privacy,
        language,
    })
}

// ---------------------------------------------------------------------------
// Validation & field rendering
// ---------------------------------------------------------------------------

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Whether `raw` is a field value the verifier should fetch: a
/// well-formed linkable URL whose scheme is in `valid_url_schemes`.
fn is_url_field(config: &Config, raw: &str) -> bool {
    is_linkable_url(raw)
        && url::Url::parse(raw)
            .map(|u| {
                config
                    .content
                    .valid_url_schemes
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(u.scheme()))
            })
            .unwrap_or(false)
}

/// The raw value of a stored entry — `value_raw` in the current shape,
/// `value` for rows written before it existed.
fn entry_raw(entry: &Value) -> Option<&str> {
    entry
        .get("value_raw")
        .or_else(|| entry.get("value"))
        .and_then(Value::as_str)
}

/// Validate field inputs without touching storage: `max_profile_fields`
/// counts *non-empty* entries (all-blank rows are dropped, matching
/// Mastodon), and name/value char limits apply after trimming.
fn validate_fields(config: &Config, inputs: &[FieldInput]) -> Result<()> {
    let max = config.content.max_profile_fields as usize;
    let mut kept = 0;
    for input in inputs {
        let name = input.name.trim();
        let raw = input.value.trim();
        if name.is_empty() && raw.is_empty() {
            continue;
        }
        kept += 1;
        if kept > max {
            return Err(Error::Validation(format!(
                "at most {max} profile fields are allowed"
            )));
        }
        if char_len(name) > config.content.max_profile_field_name_length as usize {
            return Err(Error::Validation(format!(
                "field name exceeds the {} character limit",
                config.content.max_profile_field_name_length
            )));
        }
        if char_len(raw) > config.content.max_profile_field_value_length as usize {
            return Err(Error::Validation(format!(
                "field value exceeds the {} character limit",
                config.content.max_profile_field_value_length
            )));
        }
    }
    Ok(())
}

/// Render and stamp the submitted fields into the stored shape,
/// preserving `verified_at` for entries whose `name`+`value_raw` pair
/// survives anywhere in the previous list — a reorder shouldn't cost a
/// field its stamp. Assumes [`validate_fields`] already ran.
fn render_fields(config: &Config, inputs: &[FieldInput], previous: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        let name = input.name.trim();
        let raw = input.value.trim();
        // A fully-empty entry is skipped, matching Mastodon.
        if name.is_empty() && raw.is_empty() {
            continue;
        }
        let url_valued = is_url_field(config, raw);
        let value = if url_valued {
            render_verified_link(raw, None)
        } else {
            render_bio_html(raw)
        };
        let verified_at = if url_valued {
            previous
                .iter()
                .find(|p| {
                    p.get("name").and_then(Value::as_str) == Some(name) && entry_raw(p) == Some(raw)
                })
                .and_then(|p| p.get("verified_at").cloned())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        out.push(json!({
            "name": name,
            "value": value,
            "value_raw": raw,
            "verified_at": verified_at,
        }));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// update_credentials
// ---------------------------------------------------------------------------

/// How long a replaced/cleared profile image stays in the store after
/// the row stops referencing it. Avatar/header URLs are served
/// `immutable` for a year and remote servers re-fetch them only after
/// the actor `Update` lands, so an immediate delete would 404 the old
/// URL for late consumers; a day comfortably covers delivery retries.
const STALE_PROFILE_IMAGE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Apply a normalized PATCH to the account row under a row lock, then
/// fire the post-commit effects: the actor `Update` fan-out and a
/// `VerifyProfileLinks` enqueue when URL fields changed.
pub async fn update_profile(
    state: &AppState,
    auth: &AuthContext,
    mut params: UpdateCredentials,
) -> Result<()> {
    let config = &state.config();

    // Cheap validation first — a trivially-invalid PATCH shouldn't pay
    // for upload processing.
    if let Some(s) = &params.display_name {
        if char_len(s.trim()) > config.content.max_display_name_length as usize {
            return Err(Error::Validation(format!(
                "display name exceeds the {} character limit",
                config.content.max_display_name_length
            )));
        }
    }
    if let Some(note) = &params.note {
        if char_len(note) > config.content.max_bio_length as usize {
            return Err(Error::Validation(format!(
                "note exceeds the {} character limit",
                config.content.max_bio_length
            )));
        }
    }
    if let Some(inputs) = &params.fields {
        validate_fields(config, inputs)?;
    }
    // Epic-10 posting defaults — cheap enum checks before any upload
    // work; invalid `source.*` must abort the whole PATCH.
    if let Patch::Set(v) = &mut params.privacy {
        *v = v.to_ascii_lowercase();
        settings::validate_visibility(v)?;
    }
    if let Patch::Set(l) = &mut params.language {
        *l = settings::normalize_locale(l);
        settings::validate_supported_locale("source[language]", l)?;
    }

    // Upload processing is CPU + filesystem — run it before the row
    // lock is taken. A failed DB write can orphan the stored file;
    // epic 18's sweep owns cleanup.
    let avatar = match params.avatar {
        MediaOp::Unchanged => None,
        MediaOp::Clear => Some(None),
        MediaOp::Set(bytes) => Some(Some(
            media::store_profile_image(
                config,
                &state.media,
                MediaKind::Avatar,
                auth.account_id,
                bytes,
            )
            .await?,
        )),
    };
    let header = match params.header {
        MediaOp::Unchanged => None,
        MediaOp::Clear => Some(None),
        MediaOp::Set(bytes) => Some(Some(
            media::store_profile_image(
                config,
                &state.media,
                MediaKind::Header,
                auth.account_id,
                bytes,
            )
            .await?,
        )),
    };

    // Row lock: PATCH and the verify sweep both rewrite
    // `profile_fields` — the lock keeps either writer from losing the
    // other's update.
    let mut tx = state.db.begin().await?;
    let row = sqlx::query(
        "SELECT username, display_name, bio, avatar_path, banner_path, \
                avatar_blurhash, header_blurhash, profile_fields, \
                locked, bot, discoverable, hide_network, \
                default_visibility, default_language, default_sensitive \
         FROM accounts WHERE id = $1 AND domain IS NULL FOR UPDATE",
    )
    .bind(auth.account_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::NotFound("account".into()))?;

    let username: String = row.get("username");

    let display_name = match &params.display_name {
        None => row.get::<Option<String>, _>("display_name"),
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    };
    let bio = match &params.note {
        None => row.get::<Option<String>, _>("bio"),
        Some(note) => {
            if note.trim().is_empty() {
                None
            } else {
                Some(note.clone())
            }
        }
    };
    let previous_fields: Vec<Value> = row
        .get::<Value, _>("profile_fields")
        .as_array()
        .cloned()
        .unwrap_or_default();
    let fields = match &params.fields {
        None => row.get::<Value, _>("profile_fields"),
        Some(inputs) => Value::Array(render_fields(config, inputs, &previous_fields)?),
    };
    let locked = params.locked.resolve(row.get("locked"), false);
    let bot = params.bot.resolve(row.get("bot"), false);
    let discoverable = params.discoverable.resolve(row.get("discoverable"), true);
    // Epic-10 account-level settings sharing this transaction.
    let hide_network = params
        .hide_collections
        .resolve(row.get("hide_network"), false);
    let default_visibility = params
        .privacy
        .resolve(row.get("default_visibility"), "public".to_string());
    let default_language = params
        .language
        .resolve_nullable(row.get("default_language"));
    let default_sensitive = params
        .sensitive
        .resolve(row.get("default_sensitive"), false);
    // `indexable` lives on `users` — lock it after `accounts` (the same
    // order `settings::save` uses) only when the flag is patched.
    let indexable = if params.indexable.is_unset() {
        None
    } else {
        let user = sqlx::query("SELECT indexable FROM users WHERE id = $1 FOR UPDATE")
            .bind(auth.user_id)
            .fetch_one(&mut *tx)
            .await?;
        Some(
            params
                .indexable
                .resolve(user.get::<bool, _>("indexable"), true),
        )
    };

    let old_avatar_path: Option<String> = row.get("avatar_path");
    let old_banner_path: Option<String> = row.get("banner_path");
    let avatar_path = avatar
        .as_ref()
        .map(|op| op.as_ref().map(|s| s.key.clone()))
        .unwrap_or_else(|| old_avatar_path.clone());
    let avatar_blurhash = avatar
        .as_ref()
        .map(|op| op.as_ref().map(|s| s.blurhash.clone()))
        .unwrap_or_else(|| row.get::<Option<String>, _>("avatar_blurhash"));
    let banner_path = header
        .as_ref()
        .map(|op| op.as_ref().map(|s| s.key.clone()))
        .unwrap_or_else(|| old_banner_path.clone());
    let header_blurhash = header
        .as_ref()
        .map(|op| op.as_ref().map(|s| s.blurhash.clone()))
        .unwrap_or_else(|| row.get::<Option<String>, _>("header_blurhash"));

    // AP Update only when the visible profile actually changed.
    // `hide_network` stays out: it is local-only (not in the AP actor
    // document), so toggling it must not fan out an actor Update.
    let changed = display_name != row.get::<Option<String>, _>("display_name")
        || bio != row.get::<Option<String>, _>("bio")
        || fields != row.get::<Value, _>("profile_fields")
        || avatar_path != old_avatar_path
        || banner_path != old_banner_path
        || locked != row.get::<bool, _>("locked")
        || bot != row.get::<bool, _>("bot")
        || discoverable != row.get::<bool, _>("discoverable");

    // Enqueue verification only when a URL-valued field was added or
    // its value changed — an unrelated display-name edit shouldn't
    // re-fetch unchanged pages.
    let previous_urls: HashSet<(String, String)> = previous_fields
        .iter()
        .filter_map(|e| {
            let name = e.get("name").and_then(Value::as_str)?.to_string();
            let raw = entry_raw(e)?.to_string();
            is_url_field(config, &raw).then_some((name, raw))
        })
        .collect();
    let urls_changed = fields
        .as_array()
        .map(|entries| {
            entries.iter().any(|e| {
                let name = e.get("name").and_then(Value::as_str).unwrap_or_default();
                match entry_raw(e) {
                    Some(raw) if is_url_field(config, raw) => {
                        !previous_urls.contains(&(name.to_string(), raw.to_string()))
                    }
                    _ => false,
                }
            })
        })
        .unwrap_or(false);

    sqlx::query(
        "UPDATE accounts SET display_name = $2, bio = $3, profile_fields = $4, \
                avatar_path = $5, avatar_blurhash = $6, banner_path = $7, \
                header_blurhash = $8, locked = $9, bot = $10, \
                discoverable = $11, hide_network = $12, \
                default_visibility = $13, default_language = $14, \
                default_sensitive = $15, updated_at = now() \
         WHERE id = $1",
    )
    .bind(auth.account_id)
    .bind(&display_name)
    .bind(&bio)
    .bind(&fields)
    .bind(&avatar_path)
    .bind(&avatar_blurhash)
    .bind(&banner_path)
    .bind(&header_blurhash)
    .bind(locked)
    .bind(bot)
    .bind(discoverable)
    .bind(hide_network)
    .bind(&default_visibility)
    .bind(&default_language)
    .bind(default_sensitive)
    .execute(&mut *tx)
    .await?;
    if let Some(indexable) = indexable {
        sqlx::query("UPDATE users SET indexable = $2, updated_at = now() WHERE id = $1")
            .bind(auth.user_id)
            .bind(indexable)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // A replaced or cleared profile image is unreferenced the moment
    // the row moves to the new key — but its URL was served with a
    // year-long `immutable` cache and remote servers may not have
    // processed the actor `Update` yet, so the delete is deferred by a
    // grace delay (the job re-checks the reference before removing).
    // No sweep covers `avatars/`/`headers/`, so without the enqueue
    // the object leaks. Best-effort: a failed enqueue leaves an
    // orphan — warn and carry on. The prefix guard keeps a
    // legacy/foreign path shape out of the job's reach.
    for (old, new) in [
        (old_avatar_path, &avatar_path),
        (old_banner_path, &banner_path),
    ] {
        let Some(old) = old else { continue };
        if new.as_deref() == Some(old.as_str()) {
            continue;
        }
        if !old.starts_with("avatars/") && !old.starts_with("headers/") {
            continue;
        }
        if let Err(e) = state
            .dispatcher
            .enqueue_delayed(
                JobKind::DeleteMediaObject,
                json!({ "key": old }),
                STALE_PROFILE_IMAGE_GRACE,
            )
            .await
        {
            tracing::warn!(key = %old, error = %e, "stale profile image delete enqueue failed");
        }
    }

    if changed {
        if let Some(ap) = &state.ap {
            if let Err(e) = crate::activitypub::delivery::publish_actor_update(ap, &username).await
            {
                tracing::error!(error = %e, username = %username, "actor Update publish failed");
            }
        }
    }
    if urls_changed {
        enqueue_verification(state, auth.account_id).await?;
    }
    Ok(())
}

/// Deduplicated enqueue: a 60 s NX flag keeps rapid successive edits
/// from queueing redundant fetches (the job itself is idempotent).
async fn enqueue_verification(state: &AppState, account_id: i64) -> Result<()> {
    let key = state
        .redis
        .key(&format!("verify-profile-links:{account_id}"));
    let mut conn = state.redis.conn().await?;
    let fresh: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(60)
        .query_async(&mut *conn)
        .await?;
    if fresh.is_some() {
        state
            .dispatcher
            .enqueue(
                JobKind::VerifyProfileLinks,
                json!({ "account_id": account_id }),
            )
            .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rel="me" verification
// ---------------------------------------------------------------------------

/// Per-URL verification result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Page links back with `rel="me"` → stamp `verified_at`.
    Verified,
    /// Definitive answer the link is absent (2xx without a match, or a
    /// non-2xx response) → clear `verified_at`.
    NotVerified,
    /// Transient failure — fetch error/timeout: keep the prior stamp.
    Keep,
}

/// Case/URL-normalized comparison for `href` candidates.
fn url_eq(candidate: &str, target: &str) -> bool {
    match (url::Url::parse(candidate), url::Url::parse(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => candidate == target,
    }
}

/// Whether the fetched page body links back to either target (actor id
/// or profile URL) with `rel="me"` — Mastodon's two-target rule.
fn body_links_back(body: &str, page_url: &str, targets: &[String]) -> bool {
    let Ok(selector) = scraper::Selector::parse("a[rel~=me], link[rel~=me]") else {
        return false;
    };
    let Ok(base) = url::Url::parse(page_url) else {
        return false;
    };
    let document = scraper::Html::parse_document(body);
    for element in document.select(&selector) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        // Relative hrefs resolve against the page URL.
        let resolved = base
            .join(href)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| href.to_string());
        if targets.iter().any(|t| url_eq(&resolved, t)) {
            return true;
        }
    }
    false
}

/// Fetch `page_url` and classify the outcome. Synchronous — runs on
/// `spawn_blocking` (pubby's client is ureq-backed).
fn fetch_verdict(client: &HttpClient, page_url: &str, targets: &[String]) -> Verdict {
    let headers = HashMap::from([("Accept".to_string(), "text/html".to_string())]);
    match client.get(page_url, &headers) {
        Ok(response) if response.is_success() => {
            let body = String::from_utf8_lossy(&response.body);
            if body_links_back(&body, page_url, targets) {
                Verdict::Verified
            } else {
                Verdict::NotVerified
            }
        }
        Ok(_) => Verdict::NotVerified,
        Err(_) => Verdict::Keep,
    }
}

/// `JobKind::VerifyProfileLinks` — `rel="me"` re-verification, either
/// for one account (`{"account_id": …}`, enqueued on edit) or every
/// local account with URL-valued fields (the cron sweep). All network
/// I/O lives here; PATCH never fetches.
pub struct VerifyProfileLinksJob;

/// In-flight fetch bound per job run — one runaway sweep must not
/// fan out unbounded concurrent fetches.
const VERIFY_CONCURRENCY: usize = 4;

#[async_trait::async_trait]
impl JobHandler for VerifyProfileLinksJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let account_ids: Vec<i64> = match payload.get("account_id").and_then(Value::as_i64) {
            Some(id) => vec![id],
            None => {
                sqlx::query_scalar::<_, i64>(
                    "SELECT id FROM accounts \
                     WHERE domain IS NULL AND jsonb_array_length(profile_fields) > 0 \
                     ORDER BY id",
                )
                .fetch_all(&ctx.db)
                .await?
            }
        };
        let base_url = crate::activitypub::federation_base_url(&ctx.config);
        let timeout = Duration::from_secs(ctx.config.federation.request_timeout_seconds);
        let client = if ctx.config.federation.allow_private_targets {
            HttpClient::new_relaxed(timeout)
        } else {
            HttpClient::new(timeout)
        };
        for id in account_ids {
            if let Err(e) = verify_account(ctx, &client, &base_url, id).await {
                tracing::warn!(error = %e, account_id = id, "profile-link verification failed");
            }
        }
        Ok(())
    }
}

/// Verify one local account's URL-valued fields and write the stamps
/// back under a row lock.
async fn verify_account(
    ctx: &JobContext,
    client: &HttpClient,
    base_url: &str,
    account_id: i64,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT username, profile_fields FROM accounts \
         WHERE id = $1 AND domain IS NULL",
    )
    .bind(account_id)
    .fetch_optional(&ctx.db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let username: String = row.get("username");
    let entries: Vec<Value> = row
        .get::<Value, _>("profile_fields")
        .as_array()
        .cloned()
        .unwrap_or_default();

    // (name, raw) pairs captured for the fetch, plus the deduped URL set.
    let targets = vec![
        format!("{base_url}/ap/users/{username}"),
        format!("{base_url}/@{username}"),
    ];
    let mut pairs = HashSet::new();
    let mut urls: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for entry in &entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(raw) = entry_raw(entry) else {
            continue;
        };
        if !is_url_field(&ctx.config, raw) {
            continue;
        }
        pairs.insert((name.to_string(), raw.to_string()));
        if seen.insert(raw.to_string()) {
            urls.push(raw.to_string());
        }
    }
    if urls.is_empty() {
        return Ok(());
    }

    // Bounded parallel fetches — pubby's client is sync.
    let verdicts: HashMap<String, Verdict> = futures_util::stream::iter(urls)
        .map(|url| {
            let client = client.clone();
            let targets = targets.clone();
            async move {
                let u = url.clone();
                let verdict =
                    tokio::task::spawn_blocking(move || fetch_verdict(&client, &u, &targets))
                        .await
                        .unwrap_or(Verdict::Keep);
                (url, verdict)
            }
        })
        .buffer_unordered(VERIFY_CONCURRENCY)
        .collect()
        .await;

    // Guarded write-back: re-read under FOR UPDATE and only
    // touch entries still carrying the (name, raw) pair that was
    // fetched — a concurrent edit can't get a stale stamp.
    let mut tx = ctx.db.begin().await?;
    let stored: Option<Value> = sqlx::query_scalar(
        "SELECT profile_fields FROM accounts \
         WHERE id = $1 AND domain IS NULL FOR UPDATE",
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(stored) = stored else {
        tx.commit().await?;
        return Ok(());
    };
    let now = chrono::Utc::now().to_rfc3339();
    let mut updated_entries = Vec::new();
    let mut touched = false;
    // (name, raw) pairs still present on the row — used to detect an
    // edit that landed during the fetch window.
    let mut stored_pairs = HashSet::new();
    for mut entry in stored.as_array().cloned().unwrap_or_default() {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let raw = entry_raw(&entry).map(str::to_string);
        if let Some(raw) = raw {
            if is_url_field(&ctx.config, &raw) {
                stored_pairs.insert((name.clone(), raw.clone()));
            }
            if pairs.contains(&(name, raw.clone())) {
                let before = entry.get("verified_at").cloned().unwrap_or(Value::Null);
                match verdicts.get(&raw) {
                    Some(Verdict::Verified) => {
                        entry["verified_at"] = json!(now);
                    }
                    Some(Verdict::NotVerified) => {
                        entry["verified_at"] = Value::Null;
                    }
                    _ => {}
                }
                touched |= before != *entry.get("verified_at").unwrap_or(&Value::Null);
            }
        }
        updated_entries.push(entry);
    }
    // A concurrent PATCH changed the field set mid-flight: the stale
    // fetches are inapplicable — re-enqueue so the new pairs get a
    // pass (the 60 s dedupe flag may still be set from the edit's own
    // enqueue, so bypass it).
    let lost_race = pairs != stored_pairs;
    if touched {
        sqlx::query("UPDATE accounts SET profile_fields = $2, updated_at = now() WHERE id = $1")
            .bind(account_id)
            .bind(Value::Array(updated_entries))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    if lost_race {
        let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
        if let Err(e) = dispatcher
            .enqueue(
                JobKind::VerifyProfileLinks,
                json!({ "account_id": account_id }),
            )
            .await
        {
            tracing::warn!(error = %e, account_id, "profile-link re-enqueue failed");
        }
    }
    Ok(())
}

/// Register this epic's job handlers — independent of the federation
/// gate (the sweep only needs DB + config).
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::VerifyProfileLinks, VerifyProfileLinksJob);
}
