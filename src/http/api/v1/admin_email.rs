//! `/api/v1/yonder/admin/email_templates/{kind}` and
//! `/api/v1/yonder/admin/email/test` — the mail-template CRUD and a
//! test-send that exercises the SMTP transport inline (epic 22).
//! Admin-only. Template kinds are restricted to
//! [`crate::admin::email::EDITABLE_KINDS`].

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    admin::email,
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    moderation::audit,
    Error, Result,
};

/// Reject kinds outside the admin-editable set.
fn check_kind(kind: &str) -> Result<()> {
    if email::EDITABLE_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(Error::NotFound(format!("email template {kind:?}")))
    }
}

/// `GET /api/v1/yonder/admin/email_templates/{kind}`.
#[utoipa::path(
    get,
    path = "/api/v1/yonder/admin/email_templates/{kind}",
    tag = "admin",
    params(("kind" = String, Path, description = "verify_email | password_reset | welcome | account_status")),
    responses(
        (status = 200, description = "The template"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown kind", body = crate::error::ErrorBody),
    )
)]
pub async fn get_template(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(kind): Path<String>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    check_kind(&kind)?;
    let Some((subject, text_body, html_body)) = email::template(&state.db, &kind).await? else {
        return Err(Error::NotFound(format!("email template {kind:?}")));
    };
    Ok(Json(json!({
        "kind": kind,
        "subject": subject,
        "text_body": text_body,
        "html_body": html_body,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TemplateBody {
    /// Rendered subject line (`{{var}}` placeholders allowed).
    pub subject: String,
    /// Plain-text body.
    pub text_body: String,
    /// Optional HTML body.
    pub html_body: Option<String>,
}

/// `PUT /api/v1/yonder/admin/email_templates/{kind}` — upsert.
#[utoipa::path(
    put,
    path = "/api/v1/yonder/admin/email_templates/{kind}",
    tag = "admin",
    request_body(content_type = "application/json"),
    params(("kind" = String, Path)),
    responses(
        (status = 200, description = "Updated template"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "Unknown kind", body = crate::error::ErrorBody),
    )
)]
pub async fn put_template(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(kind): Path<String>,
    Json(body): Json<TemplateBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    check_kind(&kind)?;
    sqlx::query(
        "INSERT INTO email_templates (kind, subject, text_body, html_body) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (kind) DO UPDATE SET subject = EXCLUDED.subject, \
             text_body = EXCLUDED.text_body, html_body = EXCLUDED.html_body, \
             updated_at = now()",
    )
    .bind(&kind)
    .bind(&body.subject)
    .bind(&body.text_body)
    .bind(&body.html_body)
    .execute(&state.db)
    .await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "email_template.update",
        "email_template",
        None,
        json!({ "kind": kind }),
    )
    .await?;
    Ok(Json(json!({
        "kind": kind,
        "subject": body.subject,
        "text_body": body.text_body,
        "html_body": body.html_body,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct TestBody {
    /// Recipient; defaults to the calling admin's email.
    pub to: Option<String>,
}

/// `POST /api/v1/yonder/admin/email/test` — render the `welcome`
/// template and send it through the SMTP transport inline, so
/// misconfiguration surfaces as the request error rather than a
/// dead-lettered job.
#[utoipa::path(
    post,
    path = "/api/v1/yonder/admin/email/test",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "Test mail sent"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Mail not configured / bad recipient", body = crate::error::ErrorBody),
    )
)]
pub async fn send_test(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<TestBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin")?;
    let to = match body.to {
        Some(to) => to,
        None => {
            sqlx::query_scalar::<_, Option<String>>("SELECT email FROM users WHERE account_id = $1")
                .bind(auth.account_id)
                .fetch_optional(&state.db)
                .await?
                .flatten()
                .ok_or_else(|| Error::Validation("no recipient email".into()))?
        }
    };
    let mut vars = serde_json::Map::new();
    vars.insert("instance_name".into(), json!(state.config().instance.name));
    vars.insert(
        "user_handle".into(),
        json!(format!("user {}", auth.account_id)),
    );
    let (subject, text, html) = email::template(&state.db, "welcome")
        .await?
        .ok_or_else(|| Error::NotFound("welcome template".into()))?;
    let html_rendered = html.as_deref().map(|h| email::render_html(h, &vars));
    email::send_now(
        &state.config().email,
        &to,
        &email::render(&subject, &vars),
        &email::render(&text, &vars),
        html_rendered.as_deref(),
    )
    .await?;
    audit::record(
        &state.db,
        Some(auth.account_id),
        "email.test",
        "account",
        Some(auth.account_id),
        json!({}),
    )
    .await?;
    Ok(Json(json!({ "sent": true, "to": to })))
}
