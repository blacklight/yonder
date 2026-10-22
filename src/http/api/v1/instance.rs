//! `/api/v1/instance` — basic instance information built from config
//! and live counts.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};
use sqlx::Row;

use crate::{config::RegistrationMode, http::AppState, Result};

/// `GET /api/v1/instance` — public, `MaybeAuth`-eligible.
#[utoipa::path(
    get,
    path = "/api/v1/instance",
    tag = "instance",
    responses((status = 200, description = "Instance information"))
)]
pub async fn instance(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let config = &state.config;
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
        "registrations": {
            "enabled": !matches!(mode, RegistrationMode::Closed),
            "approval_required": matches!(mode, RegistrationMode::ApprovalRequired),
            "message": config.auth.registration_message,
        },
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
        },
        "contact_account": Value::Null,
        "rules": config.instance.rules,
    })))
}
