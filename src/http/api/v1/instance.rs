//! `/api/v1/instance` — basic instance information built from config
//! and live counts. Also serves the configured branding assets
//! (`instance.logo_path`/`banner_path`) the `/about` page and the
//! `thumbnail`/`icon` entity fields reference.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{config::RegistrationMode, http::AppState, Error, Result};

/// Mastodon Rule entities for `instance.rules` — clients render the
/// numbered list on the registration and /about surfaces.
pub(crate) fn rules_json(rules: &[String]) -> Value {
    json!(rules
        .iter()
        .enumerate()
        .map(|(i, text)| json!({"id": (i + 1).to_string(), "text": text}))
        .collect::<Vec<_>>())
}

/// `{base}/api/v1/instance/{name}` when the matching `instance.*_path`
/// is configured, else `None` — the assets route 404s for unset
/// paths, so the entity only advertises URLs that resolve.
pub(crate) fn asset_url(base_url: &str, path: &Option<String>, name: &str) -> Option<String> {
    path.as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(|_| format!("{}/api/v1/instance/{name}", base_url.trim_end_matches('/')))
}

/// The `pleroma` block shared by the v1 and v2 instance entities —
/// Akkoma-style capability metadata (`fields_limits`, the VAPID
/// discovery key) a client can read regardless of which entity shape
/// it fetches.
pub(crate) fn pleroma_metadata(config: &crate::config::Config) -> Value {
    json!({
        "metadata": {
            "features": ["pleroma_emoji_reactions", "yonder"],
            "post_formats": ["text/plain", "text/markdown"],
            // Akkoma-style WebPush discovery: clients need the VAPID
            // public key *before* they can create a subscription
            // (`GET /api/v1/push/subscription` only reports it once
            // one exists).
            "vapid_public_key": config.http.vapid_public_key,
            "fields_limits": {
                "max_fields": config.content.max_profile_fields,
                "max_remote_fields": config.content.max_profile_fields,
                "name_length": config.content.max_profile_field_name_length,
                "value_length": config.content.max_profile_field_value_length,
            },
        },
    })
}

/// Resolve `instance.contact_url` to a local Account entity for
/// `contact.account` (v2) / `contact_account` (v1). Only `/@user` and
/// `/users/{user}` URLs on this instance's own host resolve — remote
/// or unparseable values stay `null`, the shape Mastodon uses for
/// "no contact account".
pub(crate) async fn contact_account(
    db: &sqlx::PgPool,
    config: &crate::config::Config,
    base_url: &str,
) -> Value {
    let Some(contact) = config.instance.contact_url.as_deref() else {
        return Value::Null;
    };
    let (Ok(contact), Ok(base)) = (
        url::Url::parse(contact),
        url::Url::parse(base_url.trim_end_matches('/')),
    ) else {
        return Value::Null;
    };
    if contact.host_str() != base.host_str() {
        return Value::Null;
    }
    let segments: Vec<&str> = contact
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    let username = match segments.as_slice() {
        [acct] if acct.starts_with('@') => &acct[1..],
        ["users", user] => *user,
        _ => return Value::Null,
    };
    match crate::users::account::load_local_by_username(db, username).await {
        Ok(Some(account)) => crate::users::account::to_account(&account, base_url, config),
        _ => Value::Null,
    }
}

/// `GET /api/v1/instance` — public, `MaybeAuth`-eligible.
#[utoipa::path(
    get,
    path = "/api/v1/instance",
    tag = "instance",
    responses((status = 200, description = "Instance information"))
)]
pub async fn instance(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let config = &state.config();
    let base_url = crate::activitypub::federation_base_url(config);
    let row = sqlx::query(
        "SELECT \
            (SELECT count(*) FROM accounts a JOIN users u ON u.account_id = a.id \
             WHERE a.domain IS NULL AND a.suspended_at IS NULL) AS user_count, \
            (SELECT count(*) FROM statuses WHERE local) AS status_count, \
            (SELECT count(DISTINCT domain) FROM accounts WHERE domain IS NOT NULL) AS domain_count",
    )
    .fetch_one(&state.db)
    .await?;

    let mode = config.auth.registration_mode;
    Ok(Json(json!({
        "uri": config.instance.domain,
        "title": config.instance.name,
        "description": config.instance.description,
        "short_description": config.instance.short_description,
        "email": config.instance.contact_email,
        "version": format!("Yonder/{}", env!("CARGO_PKG_VERSION")),
        "urls": {"streaming_api": format!("wss://{}/api/v1/streaming", config.instance.domain)},
        "thumbnail": asset_url(&base_url, &config.instance.banner_path, "banner"),
        "stats": {
            "user_count": row.get::<i64, _>("user_count"),
            "status_count": row.get::<i64, _>("status_count"),
            "domain_count": row.get::<i64, _>("domain_count"),
        },
        "languages": config.instance.languages,
        // Mastodon's v1 Instance entity uses scalar booleans; the
        // object-shaped `registrations` block lives on /api/v2/instance.
        "registrations": !matches!(mode, RegistrationMode::Closed),
        "approval_required": matches!(mode, RegistrationMode::ApprovalRequired),
        "invites_enabled": config.auth.invite_links_enabled(),
        "configuration": {
            "statuses": {
                "max_characters": config.content.max_post_length,
                "max_media_attachments": config.media.max_attachments_per_post,
            },
            "media_attachments": {
                "image_size_limit": config.media.max_upload_size,
            },
            "polls": {
                "max_options": config.polls.max_options,
                "max_characters_per_option": config.polls.max_option_chars,
                "min_expiration": 300,
                "max_expiration": config.polls.max_duration_seconds,
            },
            "translation": {"enabled": crate::translation::enabled(&config.content)},
        },
        "contact_account": contact_account(&state.db, config, &base_url).await,
        "rules": rules_json(&config.instance.rules),
        // Akkoma-style capability block: its presence makes clients
        // (Moshidon et al.) treat us as an Akkoma-compatible server —
        // `post_formats` advertises the content_type values
        // `POST /statuses` accepts (markdown is rendered server-side),
        // `pleroma_emoji_reactions` points them at the
        // `/api/v1/pleroma/statuses/{id}/reactions` endpoints.
        "pleroma": pleroma_metadata(config),
    })))
}

/// `GET /api/v1/instance/extended_description` — the long-form "about
/// this server" text. 404 when unset, like Mastodon.
#[utoipa::path(
    get,
    path = "/api/v1/instance/extended_description",
    tag = "instance",
    responses(
        (status = 200, description = "ExtendedDescription entity"),
        (status = 404, description = "No extended description", body = crate::error::ErrorBody),
    )
)]
pub async fn extended_description(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let config = state.config();
    let content = config
        .instance
        .extended_description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| Error::NotFound("extended_description".into()))?;
    Ok(Json(json!({
        "updated_at": chrono::Utc::now().to_rfc3339(),
        "content": content,
    })))
}

/// `GET /api/v1/instance/peers` — the domains this server is aware
/// of (Mastodon-compatible): remote account domains ∪ delivery
/// targets ∪ configured relays, minus silence/suspend-blocked
/// domains — Mastodon's `Instance.searchable` semantics keep
/// moderated names from leaking through the listing. Gated by
/// `instance.peers_api_enabled` — off means 404.
#[utoipa::path(
    get,
    path = "/api/v1/instance/peers",
    tag = "instance",
    responses(
        (status = 200, description = "Array of known remote domains"),
        (status = 404, description = "Listing disabled", body = crate::error::ErrorBody),
    )
)]
pub async fn peers(State(state): State<Arc<AppState>>) -> Result<Json<Vec<String>>> {
    if !state.config().instance.peers_api_enabled {
        return Err(Error::NotFound("peers".into()));
    }
    let domains = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT d FROM ( \
             SELECT domain AS d FROM accounts WHERE domain IS NOT NULL \
             UNION \
             SELECT lower(substring(inbox_url from '://([^/:]+)')) FROM relays \
             UNION \
             SELECT lower(substring(inbox_url from '://([^/:]+)')) FROM deliveries \
         ) sub \
         WHERE d IS NOT NULL \
           AND d ~ '^[a-z0-9][a-z0-9.-]*$' \
           AND NOT EXISTS ( \
               SELECT 1 FROM domain_blocks b \
               WHERE b.domain = sub.d AND b.severity IN ('silence', 'suspend') \
           ) \
         ORDER BY d",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(domains))
}

/// `GET /api/v1/instance/domain_blocks` — the public
/// moderated-instances listing (Mastodon-compatible): domain,
/// severity, `public_comment`; `obfuscate` masks the domain. Gated
/// by `instance.expose_moderated_instances` — off means 404.
#[utoipa::path(
    get,
    path = "/api/v1/instance/domain_blocks",
    tag = "instance",
    responses(
        (status = 200, description = "Array of public DomainBlock"),
        (status = 404, description = "Listing disabled", body = crate::error::ErrorBody),
    )
)]
pub async fn domain_blocks(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    if !state.config().instance.expose_moderated_instances {
        return Err(Error::NotFound("domain_blocks".into()));
    }
    let rows = crate::moderation::blocks::list_public(&state.db).await?;
    Ok(Json(json!(rows
        .iter()
        .map(|b| json!({
            "domain": b.domain,
            "severity": b.severity.as_str(),
            "comment": b.public_comment,
        }))
        .collect::<Vec<_>>())))
}

/// `GET /api/v1/instance/logo` — the `instance.logo_path` image,
/// advertised as `icon` in the v2 entity. 404 when unset.
#[utoipa::path(
    get,
    path = "/api/v1/instance/logo",
    tag = "instance",
    responses(
        (status = 200, description = "Instance logo image"),
        (status = 404, description = "No logo configured", body = crate::error::ErrorBody),
    )
)]
pub async fn logo(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse> {
    serve_asset(state.config().instance.logo_path.as_deref()).await
}

/// `GET /api/v1/instance/banner` — the `instance.banner_path` image,
/// advertised as `thumbnail` in both entities. 404 when unset.
#[utoipa::path(
    get,
    path = "/api/v1/instance/banner",
    tag = "instance",
    responses(
        (status = 200, description = "Instance banner image"),
        (status = 404, description = "No banner configured", body = crate::error::ErrorBody),
    )
)]
pub async fn banner(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse> {
    serve_asset(state.config().instance.banner_path.as_deref()).await
}

/// Read the admin-configured file off disk on every request — an
/// overlay path change or a replaced file takes effect without a
/// restart. The path itself is trusted (admin-only config), but only
/// known image extensions get an image MIME.
async fn serve_asset(path: Option<&str>) -> Result<impl IntoResponse> {
    let path = path
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| Error::NotFound("instance_asset".into()))?;
    let bytes = tokio::fs::read(path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::NotFound("instance_asset".into()),
        _ => Error::Io(e),
    })?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        asset_content_type(path)
            .and_then(|ct| HeaderValue::from_str(ct).ok())
            .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream")),
    );
    if asset_content_type(path) == Some("image/svg+xml") {
        // An SVG is a document — a direct navigation would run its
        // embedded script on the instance origin.
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'"),
        );
    }
    // The URL is stable while the file beneath it can change —
    // short public cache, never `immutable`.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok((StatusCode::OK, headers, bytes))
}

/// MIME by file extension — raster image types plus SVG/ICO.
fn asset_content_type(path: &str) -> Option<&'static str> {
    match path.rsplit('.').next()? {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "svg" => Some("image/svg+xml"),
        "avif" => Some("image/avif"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}
