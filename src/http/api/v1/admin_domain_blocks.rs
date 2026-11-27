//! `/api/v1/admin/domain_blocks*` — instance moderation CRUD
//! (epic 21), plus the public `/api/v1/instance/domain_blocks`
//! listing. Admin-only (`require_scope` — domain moderation is not a
//! moderator power in Mastodon either).
//!
//! Field vocabulary mirrors Mastodon's `Admin::DomainBlock` exactly:
//! `severity` (`noop|silence|suspend`), `reject_media`,
//! `reject_reports`, `obfuscate`, `private_comment`, `public_comment`.
//! POST is an upsert on `domain` (normalized with the pubby
//! canonicalizer — `https://Example.com:443/x` and `example.com`
//! collide), PATCH edits by id.

use std::sync::Arc;

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    moderation::{
        blocks::{self, BlockRow, Severity},
        csv as domain_csv,
    },
    Error, Result,
};

/// `POST /api/v1/admin/domain_blocks` — create or update a block.
#[utoipa::path(
    post,
    path = "/api/v1/admin/domain_blocks",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "The Admin::DomainBlock"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Invalid domain or severity", body = crate::error::ErrorBody),
    )
)]
pub async fn create(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Json(body): Json<BlockBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:write:domain_blocks")?;
    let domain = crate::moderation::domain::normalize_domain(&body.domain);
    if domain.is_empty() {
        return Err(Error::Validation(format!(
            "invalid domain {:?}",
            body.domain
        )));
    }
    if domain == crate::moderation::domain::normalize_domain(&state.config().instance.domain) {
        return Err(Error::Validation("cannot block your own instance".into()));
    }
    let severity = parse_severity(body.severity.as_deref())?;
    let private_comment = body.private_comment.unwrap_or_default();
    let public_comment = body.public_comment.unwrap_or_default();
    check_comments(&private_comment, &public_comment)?;
    let existed = blocks::policy_for(&state.db, &domain).await?.is_some();
    let mut tx = state.db.begin().await?;
    let row = blocks::upsert(
        &mut *tx,
        &domain,
        severity,
        body.reject_media.unwrap_or(false),
        body.reject_reports.unwrap_or(false),
        body.obfuscate.unwrap_or(false),
        &private_comment,
        &public_comment,
        auth.account_id,
    )
    .await?;
    crate::moderation::audit::record_action(
        &mut *tx,
        auth.account_id,
        if existed {
            "domain_block.update"
        } else {
            "domain_block.create"
        },
        "domain",
        row.id,
        &row.private_comment,
        json!({ "domain": row.domain, "severity": row.severity.as_str() }),
    )
    .await?;
    tx.commit().await?;
    blocks::refresh_blocked(&state).await?;
    Ok(Json(entity(&row)))
}

/// Comments are capped at 2000 chars — `private_comment` by the
/// renamed `reason` CHECK, `public_comment` by convention.
fn check_comments(private: &str, public: &str) -> Result<()> {
    if private.chars().count() > 2000 || public.chars().count() > 2000 {
        return Err(Error::Validation(
            "comments are limited to 2000 characters".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct BlockBody {
    pub domain: String,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub reject_media: Option<bool>,
    #[serde(default)]
    pub reject_reports: Option<bool>,
    #[serde(default)]
    pub obfuscate: Option<bool>,
    #[serde(default)]
    pub private_comment: Option<String>,
    #[serde(default)]
    pub public_comment: Option<String>,
}

fn parse_severity(s: Option<&str>) -> Result<Severity> {
    match s {
        None => Ok(Severity::Silence),
        Some(v) => {
            Severity::parse(v).ok_or_else(|| Error::Validation(format!("invalid severity {v:?}")))
        }
    }
}

/// `GET /api/v1/admin/domain_blocks` — all blocks, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/admin/domain_blocks",
    tag = "admin",
    responses(
        (status = 200, description = "Array of Admin::DomainBlock"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn list(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Query(q): Query<ListParams>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:read:domain_blocks")?;
    let rows = blocks::list(
        &state.db,
        q.max_id,
        q.since_id,
        q.limit.unwrap_or(100).clamp(1, 200),
    )
    .await?;
    Ok(Json(json!(rows.iter().map(entity).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub max_id: Option<i64>,
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/v1/admin/domain_blocks/{id}`.
#[utoipa::path(
    get,
    path = "/api/v1/admin/domain_blocks/{id}",
    tag = "admin",
    responses(
        (status = 200, description = "The Admin::DomainBlock"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such block", body = crate::error::ErrorBody),
    )
)]
pub async fn show(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:read:domain_blocks")?;
    Ok(Json(entity(&blocks::get(&state.db, id).await?)))
}

/// `PATCH /api/v1/admin/domain_blocks/{id}` — partial update of
/// severity/options/comments (domain itself is immutable).
#[utoipa::path(
    patch,
    path = "/api/v1/admin/domain_blocks/{id}",
    tag = "admin",
    request_body(content_type = "application/json"),
    responses(
        (status = 200, description = "The updated Admin::DomainBlock"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such block", body = crate::error::ErrorBody),
    )
)]
pub async fn update(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
    Json(body): Json<PatchBody>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:write:domain_blocks")?;
    let existing = blocks::get(&state.db, id).await?;
    check_comments(
        body.private_comment
            .as_deref()
            .unwrap_or(&existing.private_comment),
        body.public_comment
            .as_deref()
            .unwrap_or(&existing.public_comment),
    )?;
    let mut tx = state.db.begin().await?;
    let row = blocks::upsert(
        &mut *tx,
        &existing.domain,
        parse_severity(
            body.severity
                .as_deref()
                .or(Some(existing.severity.as_str())),
        )?,
        body.reject_media.unwrap_or(existing.reject_media),
        body.reject_reports.unwrap_or(existing.reject_reports),
        body.obfuscate.unwrap_or(existing.obfuscate),
        body.private_comment
            .as_deref()
            .unwrap_or(&existing.private_comment),
        body.public_comment
            .as_deref()
            .unwrap_or(&existing.public_comment),
        auth.account_id,
    )
    .await?;
    crate::moderation::audit::record_action(
        &mut *tx,
        auth.account_id,
        "domain_block.update",
        "domain",
        row.id,
        &row.private_comment,
        json!({ "domain": row.domain, "severity": row.severity.as_str() }),
    )
    .await?;
    tx.commit().await?;
    blocks::refresh_blocked(&state).await?;
    Ok(Json(entity(&row)))
}

#[derive(Deserialize)]
pub struct PatchBody {
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub reject_media: Option<bool>,
    #[serde(default)]
    pub reject_reports: Option<bool>,
    #[serde(default)]
    pub obfuscate: Option<bool>,
    #[serde(default)]
    pub private_comment: Option<String>,
    #[serde(default)]
    pub public_comment: Option<String>,
}

/// `DELETE /api/v1/admin/domain_blocks/{id}` — lift the block.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/domain_blocks/{id}",
    tag = "admin",
    responses(
        (status = 200, description = "Block removed"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 404, description = "No such block", body = crate::error::ErrorBody),
    )
)]
pub async fn delete_block(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    Path(id): Path<i64>,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:write:domain_blocks")?;
    let mut tx = state.db.begin().await?;
    if !blocks::delete(&mut *tx, id).await? {
        return Err(Error::NotFound("domain block".into()));
    }
    crate::moderation::audit::record(
        &mut *tx,
        Some(auth.account_id),
        "domain_block.delete",
        "domain",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    blocks::refresh_blocked(&state).await?;
    Ok(Json(json!({})))
}

/// `GET /api/v1/admin/domain_blocks/export` — every block as a
/// Mastodon-compatible CSV download.
#[utoipa::path(
    get,
    path = "/api/v1/admin/domain_blocks/export",
    tag = "admin",
    responses(
        (status = 200, description = "Mastodon-compatible domain block CSV", content_type = "text/csv"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
    )
)]
pub async fn export_csv(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Response> {
    require_scope(&state.config(), &auth, "admin:read:domain_blocks")?;
    let rows = blocks::list_export(&state.db).await?;
    let bytes = domain_csv::export(&rows)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"domain_blocks.csv\""),
    );
    Ok((headers, bytes).into_response())
}

fn malformed_multipart(e: impl std::fmt::Display) -> Error {
    Error::Validation(format!("malformed multipart: {e}"))
}

/// `POST /api/v1/admin/domain_blocks/import` — bulk-import a
/// Mastodon-compatible CSV (`file` or `data` multipart field). Files
/// exported by Mastodon v4.1+, headerless one-domain-per-line lists
/// and common Pleroma/Akkoma blocklist spellings are accepted.
/// Existing rows are skipped by default (Mastodon's behavior); pass
/// `overwrite=true` to update them.
#[utoipa::path(
    post,
    path = "/api/v1/admin/domain_blocks/import",
    tag = "admin",
    request_body(
        content = String,
        description = "Multipart CSV upload: file (or data), optional overwrite boolean",
        content_type = "multipart/form-data"
    ),
    responses(
        (status = 200, description = "Import counters plus row-level validation errors"),
        (status = 403, description = "Admin role required", body = crate::error::ErrorBody),
        (status = 422, description = "Malformed CSV or multipart body", body = crate::error::ErrorBody),
    )
)]
pub async fn import_csv(
    State(state): State<Arc<AppState>>,
    auth: AuthContext,
    mut multipart: Multipart,
) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "admin:write:domain_blocks")?;

    let mut filename: Option<String> = None;
    let mut upload: Option<Vec<u8>> = None;
    let mut overwrite = false;
    while let Some(field) = multipart.next_field().await.map_err(malformed_multipart)? {
        match field.name().unwrap_or("") {
            "file" | "data" => {
                filename = field.file_name().map(str::to_owned);
                upload = Some(field.bytes().await.map_err(malformed_multipart)?.to_vec());
            }
            "overwrite" => {
                let value = field.text().await.map_err(malformed_multipart)?;
                overwrite = domain_csv::parse_bool(&value).ok_or_else(|| {
                    Error::Validation(format!("invalid overwrite boolean {value:?}"))
                })?;
            }
            _ => {}
        }
    }
    let bytes = upload.ok_or_else(|| Error::Validation("a CSV file field is required".into()))?;
    if bytes.is_empty() {
        return Err(Error::Validation("the CSV file is empty".into()));
    }

    let parsed = domain_csv::parse_import(&bytes)?;
    let report = domain_csv::apply_import(
        &state.db,
        &state.config().instance.domain,
        parsed,
        overwrite,
        filename.as_deref(),
        auth.account_id,
    )
    .await?;
    if report.mutated() {
        blocks::refresh_blocked(&state).await?;
    }

    Ok(Json(report.json()))
}

/// Mastodon `Admin::DomainBlock` entity — `digest` is the SHA-256 of
/// the stored domain (Mastodon's `domain_digest`).
fn entity(row: &BlockRow) -> Value {
    use sha2::Digest;
    json!({
        "id": row.id.to_string(),
        "domain": row.domain,
        "digest": hex::encode(sha2::Sha256::digest(row.domain.as_bytes())),
        "created_at": row.created_at.to_rfc3339(),
        "severity": row.severity.as_str(),
        "reject_media": row.reject_media,
        "reject_reports": row.reject_reports,
        "private_comment": row.private_comment,
        "public_comment": row.public_comment,
        "obfuscate": row.obfuscate,
    })
}
