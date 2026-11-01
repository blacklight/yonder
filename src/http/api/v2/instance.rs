//! `/api/v2/instance` — instance metadata in the Mastodon API v2
//! shape: `domain` instead of `uri`, `configuration.urls` for
//! streaming, and `registrations` as an object (`enabled`,
//! `approval_required`, `message`, `url`). Yonder adds
//! `registrations.invites_enabled` — whether non-staff users may
//! create invite links (the v1 entity's scalar `invites_enabled`).

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{
    config::RegistrationMode,
    http::api::v1::instance::{asset_url, contact_account, rules_json},
    http::AppState,
    Result,
};

/// `GET /api/v2/instance` — public, `MaybeAuth`-eligible.
#[utoipa::path(
    get,
    path = "/api/v2/instance",
    tag = "instance",
    responses((status = 200, description = "Instance information (v2 shape)"))
)]
pub async fn instance(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let config = &state.config();
    let base_url = crate::activitypub::federation_base_url(config);
    let mode = config.auth.registration_mode;
    let thumbnail = asset_url(&base_url, &config.instance.banner_path, "banner");
    let icon = asset_url(&base_url, &config.instance.logo_path, "logo");
    let active_month: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT account_id) FROM statuses \
         WHERE local AND created_at > now() - interval '30 days'",
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "domain": config.instance.domain,
        "title": config.instance.name,
        "version": format!("Yonder/{}", env!("CARGO_PKG_VERSION")),
        "source_url": env!("CARGO_PKG_REPOSITORY"),
        "description": config.instance.description,
        "short_description": config.instance.short_description,
        "languages": config.instance.languages,
        // Mastodon 4.x v2 shapes: `thumbnail` is an object, `icon` a
        // `[{src, size}]` list — empty rather than null when no logo
        // is configured so strict clients never see the wrong type.
        "thumbnail": thumbnail.map(|u| json!({
            "url": u,
            "blurhash": Value::Null,
            "versions": {"@1x": u, "@2x": u},
        })),
        "icon": icon.map(|src| json!([{"src": src, "size": "1x"}])).unwrap_or(json!([])),
        "usage": {"users": {"active_month": active_month}},
        "registrations": {
            "enabled": !matches!(mode, RegistrationMode::Closed),
            "approval_required": matches!(mode, RegistrationMode::ApprovalRequired),
            "message": config.auth.registration_message,
            "url": Value::Null,
            "invites_enabled": config.auth.invite_links_enabled(),
        },
        "configuration": {
            "urls": {
                "streaming_api": format!("wss://{}/api/v1/streaming", config.instance.domain),
            },
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
        "contact": {
            "email": config.instance.contact_email,
            "account": contact_account(&state.db, config, &base_url).await,
            // Yonder extensions — the /about page's "administered by"
            // block uses these when `account` stays null (remote or
            // unresolvable `contact_url`).
            "name": config.instance.contact_name,
            "url": config.instance.contact_url,
        },
        "rules": rules_json(&config.instance.rules),
    })))
}
