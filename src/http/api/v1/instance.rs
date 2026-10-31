//! `/api/v1/instance` — basic instance information built from config
//! and live counts.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{config::RegistrationMode, http::AppState, Error, Result};

/// `GET /api/v1/instance` — public, `MaybeAuth`-eligible.
#[utoipa::path(
    get,
    path = "/api/v1/instance",
    tag = "instance",
    responses((status = 200, description = "Instance information"))
)]
pub async fn instance(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let config = &state.config();
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
        "contact_account": Value::Null,
        "rules": config.instance.rules,
        // Akkoma-style capability block: its presence makes clients
        // (Moshidon et al.) treat us as an Akkoma-compatible server —
        // `post_formats` advertises the content_type values
        // `POST /statuses` accepts (markdown is rendered server-side),
        // `pleroma_emoji_reactions` points them at the
        // `/api/v1/pleroma/statuses/{id}/reactions` endpoints.
        "pleroma": {
            "metadata": {
                "features": ["pleroma_emoji_reactions", "yonder"],
                "post_formats": ["text/plain", "text/markdown"],
                "fields_limits": {
                    "max_fields": config.content.max_profile_fields,
                    "max_remote_fields": config.content.max_profile_fields,
                    "name_length": config.content.max_profile_field_name_length,
                    "value_length": config.content.max_profile_field_value_length,
                },
            },
        },
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
