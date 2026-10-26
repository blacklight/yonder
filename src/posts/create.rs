//! Status create / edit / delete — the write pipeline (epic 12).
//!
//! `create_status` runs: parse+validate → idempotency reserve →
//! mention resolution → render → persist (status + uri backfill +
//! media + poll + mentions + tags + idempotent-target in one
//! transaction) → fan-out (`FetchLinkPreview`, AP `Create`).
//! A future `scheduled_at` diverts into [`scheduled`] instead.
//!
//! `Idempotency-Key` is honored by the atomic
//! `status_idempotency_keys` record: a fresh `INSERT … ON CONFLICT DO
//! NOTHING` wins the reservation (`target_id` NULL = in flight), a
//! same-fingerprint completed row replays its typed result, a
//! different fingerprint is a 422, an in-flight duplicate is a
//! 409/retry.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{http::middleware::auth::AuthContext, users::settings, Error, Result};

use super::{entity, mentions, polls, render, scheduled, PostContext, StatusRow};

/// A status create/update request as normalized params (JSON and form
/// bodies land in the same shape).
#[derive(Debug, Clone, Default)]
pub struct StatusParams {
    /// Raw source text.
    pub status: String,
    /// `text/plain` / `text/markdown`; `None` = instance default.
    pub content_type: Option<String>,
    /// `public|unlisted|private|followers_only|direct`; `None` =
    /// account default.
    pub visibility: Option<String>,
    /// BCP-47 language; `None` = resolved user default.
    pub language: Option<String>,
    /// Mark media/content sensitive; `None` = account default.
    pub sensitive: Option<bool>,
    /// Content warning text (≤ 2000 chars).
    pub spoiler_text: Option<String>,
    /// Parent status id for replies.
    pub in_reply_to_id: Option<i64>,
    /// `media_attachments` ids to attach; `None` = the field was not
    /// supplied (on edit: keep the existing attachments).
    pub media_ids: Option<Vec<i64>>,
    /// Poll definition — exclusive with `media_ids`.
    pub poll: Option<polls::PollParams>,
    /// Quoted status id (epic 13) — exclusive with `in_reply_to_id`.
    pub quote_id: Option<i64>,
    /// Future time → schedule instead of publishing.
    pub scheduled_at: Option<DateTime<Utc>>,
}

/// The validated, resolved create payload (stored verbatim in
/// `scheduled_statuses.params` for scheduled posts).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ValidatedStatus {
    /// Raw source text (may be empty when media/poll present).
    pub status: String,
    /// `text/plain|text/markdown`.
    pub content_type: String,
    /// Stored visibility (`public|unlisted|followers_only|direct`).
    pub visibility: String,
    /// BCP-47 language (resolved through the user default chain).
    pub language: Option<String>,
    /// Sensitive flag.
    pub sensitive: bool,
    /// CW text, if any.
    pub spoiler_text: Option<String>,
    /// Parent status (validated to exist, be visible).
    pub in_reply_to_id: Option<i64>,
    /// Media ids to attach (already ownership-checked); `None` keeps
    /// the status's current attachments — meaningful only on edit.
    pub media_ids: Option<Vec<i64>>,
    /// Validated poll.
    pub poll: Option<polls::PollParams>,
    /// Resolved quote target (validated to exist, be visible; boost
    /// rows normalized to the original).
    #[serde(default)]
    pub quote_of_id: Option<i64>,
}

/// What a create produced.
pub enum CreateOutcome {
    /// An immediate post — the Status entity JSON.
    Published(Value),
    /// A scheduled post — the ScheduledStatus entity JSON.
    Scheduled(Value),
}

const SPOILER_MAX: usize = 2000;

/// Parse+validate `params` against config, settings defaults and the
/// database (`in_reply_to` visibility, media ownership).
/// `editing_status_id` is the status being edited (if any) — media
/// already attached to it may be re-submitted.
pub async fn validate(
    ctx: &PostContext,
    auth: &AuthContext,
    params: &StatusParams,
    editing_status_id: Option<i64>,
) -> Result<ValidatedStatus> {
    let config = &ctx.config;
    let user_settings = settings::load(&ctx.db, auth.user_id).await?;

    let content_type = params.content_type.clone().unwrap_or_else(|| {
        match config.content.default_post_content_type {
            crate::config::PostContentType::TextPlain => "text/plain".to_string(),
            crate::config::PostContentType::TextMarkdown => "text/markdown".to_string(),
        }
    });
    if !matches!(content_type.as_str(), "text/plain" | "text/markdown") {
        return Err(Error::Validation(format!(
            "content_type must be text/plain or text/markdown (got {content_type:?})"
        )));
    }
    if params.status.chars().count() > config.content.max_post_length as usize {
        return Err(Error::Validation(format!(
            "status is too long (at most {} characters)",
            config.content.max_post_length
        )));
    }
    let has_media = params.media_ids.as_ref().is_some_and(|ids| !ids.is_empty());
    if params.poll.is_some() && has_media {
        return Err(Error::Validation(
            "a status is either poll or media, not both".to_string(),
        ));
    }
    if params.status.trim().is_empty() && !has_media && params.poll.is_none() {
        return Err(Error::Validation("status cannot be empty".to_string()));
    }

    let visibility = match &params.visibility {
        Some(v) => super::normalize_visibility(v)?,
        None => super::default_visibility(&user_settings.default_visibility).to_string(),
    };
    let language = match &params.language {
        Some(l) if !l.trim().is_empty() => Some(l.trim().to_string()),
        _ => Some(settings::effective_language(
            user_settings.default_language.as_deref(),
            user_settings.locale.as_deref(),
            None,
            config,
        )),
    };
    // `force_sensitive` (moderation `sensitive` action) overrides both
    // the caller's flag and the account default.
    let force_sensitive: bool =
        sqlx::query_scalar("SELECT force_sensitive FROM accounts WHERE id = $1")
            .bind(auth.account_id)
            .fetch_one(&ctx.db)
            .await?;
    let sensitive = force_sensitive || params.sensitive.unwrap_or(user_settings.default_sensitive);
    let spoiler_text = params.spoiler_text.clone().filter(|s| !s.trim().is_empty());
    if let Some(s) = &spoiler_text {
        if s.chars().count() > SPOILER_MAX {
            return Err(Error::Validation(format!(
                "spoiler_text must be at most {SPOILER_MAX} characters"
            )));
        }
    }
    if params.in_reply_to_id.is_some() && params.quote_id.is_some() {
        return Err(Error::Validation(
            "a status is either a reply or a quote, not both".to_string(),
        ));
    }
    if let Some(parent_id) = params.in_reply_to_id {
        match super::load(&ctx.db, parent_id).await? {
            Some(parent) if super::can_see(&ctx.db, &parent, Some(auth.account_id)).await? => {}
            _ => return Err(Error::NotFound("in_reply_to status".into())),
        }
    }
    // A quote_id on edit is validated but not applied — Mastodon
    // forbids retargeting a quote post-hoc.
    let quote_of_id = match params.quote_id {
        Some(qid) => Some(
            crate::interactions::quote::validate_target(ctx, qid, auth.account_id)
                .await?
                .id,
        ),
        None => None,
    };
    if let Some(poll) = &params.poll {
        polls::validate(ctx, poll)?;
    }
    let cap = config.media.max_attachments_per_post as usize;
    // Dedupe, preserving order — a repeated id would otherwise double
    // the attachment and violate `scheduled_status_media`'s PK.
    let media_ids: Option<Vec<i64>> = params.media_ids.as_ref().map(|ids| {
        let mut dedup: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            if !dedup.contains(id) {
                dedup.push(*id);
            }
        }
        dedup
    });
    if let Some(ids) = &media_ids {
        if ids.len() > cap {
            return Err(Error::Validation(format!(
                "a status attaches at most {cap} media"
            )));
        }
        for media_id in ids {
            check_media_claimable(ctx, auth.account_id, *media_id, editing_status_id).await?;
        }
    }

    Ok(ValidatedStatus {
        status: params.status.clone(),
        content_type,
        visibility,
        language,
        sensitive,
        spoiler_text,
        in_reply_to_id: params.in_reply_to_id,
        media_ids,
        poll: params.poll.clone(),
        quote_of_id,
    })
}

/// An attachment id may be attached iff it's the author's own, not
/// attached to another status (media already on `editing_status_id`
/// may be re-submitted on edit), and not reserved by a scheduled
/// status.
pub async fn check_media_claimable(
    ctx: &PostContext,
    account_id: i64,
    media_id: i64,
    editing_status_id: Option<i64>,
) -> Result<()> {
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT account_id, status_id FROM media_attachments WHERE id = $1")
            .bind(media_id)
            .fetch_optional(&ctx.db)
            .await?;
    match row {
        Some((owner, status_id))
            if owner == account_id && (status_id.is_none() || status_id == editing_status_id) =>
        {
            if status_id.is_some() {
                // Already attached to the status being edited — it
                // can't hold a scheduled reservation.
                return Ok(());
            }
            let reserved: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM scheduled_status_media \
                 WHERE media_attachment_id = $1)",
            )
            .bind(media_id)
            .fetch_one(&ctx.db)
            .await?;
            if reserved {
                Err(Error::Validation(format!(
                    "media {media_id} is attached to a scheduled status"
                )))
            } else {
                Ok(())
            }
        }
        Some((owner, _)) if owner != account_id => Err(Error::Forbidden(
            "media attachment belongs to a different account",
        )),
        _ => Err(Error::NotFound("media".into())),
    }
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// The outcome of an `Idempotency-Key` reservation.
enum IdemReservation {
    /// Fresh reservation — proceed; `release` on failure, `complete`
    /// in the create transaction.
    Reserved,
    /// A completed record with the same fingerprint — replay the
    /// typed target.
    Replay { target_type: String, target_id: i64 },
    /// Another request holds the reservation — retry later.
    InFlight,
    /// Same key, different payload.
    Mismatch,
}

/// Hash the *validated* payload — the replay contract compares the
/// effective request, not the raw body.
fn fingerprint(validated: &ValidatedStatus, scheduled: bool) -> String {
    let canonical = json!({
        "status": validated.status,
        "content_type": validated.content_type,
        "visibility": validated.visibility,
        "language": validated.language,
        "sensitive": validated.sensitive,
        "spoiler_text": validated.spoiler_text,
        "in_reply_to_id": validated.in_reply_to_id,
        "media_ids": validated.media_ids.as_deref().unwrap_or_default(),
        "poll": validated.poll,
        "quote_of_id": validated.quote_of_id,
        "scheduled": scheduled,
    });
    hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
}

/// Atomically reserve `key` for `account_id`; also prunes records
/// older than 24 h (cheap range delete on a small table).
async fn idem_begin(
    ctx: &PostContext,
    account_id: i64,
    key: &str,
    fingerprint: &str,
) -> Result<IdemReservation> {
    sqlx::query(
        "DELETE FROM status_idempotency_keys \
         WHERE created_at < now() - interval '24 hours'",
    )
    .execute(&ctx.db)
    .await?;
    let won: Option<i64> = sqlx::query_scalar(
        "INSERT INTO status_idempotency_keys (account_id, idempotency_key, target_type, fingerprint) \
         VALUES ($1, $2, 'status', $3) ON CONFLICT (account_id, idempotency_key) DO NOTHING \
         RETURNING id",
    )
    .bind(account_id)
    .bind(key)
    .bind(fingerprint)
    .fetch_optional(&ctx.db)
    .await?;
    if won.is_some() {
        return Ok(IdemReservation::Reserved);
    }
    let row = sqlx::query(
        "SELECT target_type, target_id, fingerprint FROM status_idempotency_keys \
         WHERE account_id = $1 AND idempotency_key = $2",
    )
    .bind(account_id)
    .bind(key)
    .fetch_one(&ctx.db)
    .await?;
    let existing_fp: String = row.get("fingerprint");
    if existing_fp != fingerprint {
        return Ok(IdemReservation::Mismatch);
    }
    match row.get::<Option<i64>, _>("target_id") {
        None => Ok(IdemReservation::InFlight),
        Some(target_id) => Ok(IdemReservation::Replay {
            target_type: row.get("target_type"),
            target_id,
        }),
    }
}

/// Back-fill the completed target inside the create transaction.
async fn idem_complete(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    key: &str,
    target_type: &str,
    target_id: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE status_idempotency_keys SET target_type = $3, target_id = $4 \
         WHERE account_id = $1 AND idempotency_key = $2",
    )
    .bind(account_id)
    .bind(key)
    .bind(target_type)
    .bind(target_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Drop a reservation after a failed create so the next attempt can
/// retry cleanly.
async fn idem_release(ctx: &PostContext, account_id: i64, key: &str) {
    let _ = sqlx::query(
        "DELETE FROM status_idempotency_keys \
         WHERE account_id = $1 AND idempotency_key = $2 AND target_id IS NULL",
    )
    .bind(account_id)
    .bind(key)
    .execute(&ctx.db)
    .await;
}

// ---------------------------------------------------------------------------
// Persist
// ---------------------------------------------------------------------------

/// Insert the status and all its relations in one transaction; returns
/// the new `statuses.id`. `resolved`/`rendered` are computed outside;
/// `scheduled_deletion_days` comes from the user settings; `idem_key`
/// back-fills the `status_idempotency_keys` record atomically.
#[allow(clippy::too_many_arguments)]
async fn insert_status(
    ctx: &PostContext,
    username: &str,
    account_id: i64,
    v: &ValidatedStatus,
    resolved: &[mentions::ResolvedMention],
    rendered: &render::RenderedPost,
    scheduled_deletion_days: i32,
    idem_key: Option<&str>,
) -> Result<i64> {
    let mut tx = ctx.db.begin().await?;
    let id = insert_status_tx(
        ctx,
        &mut tx,
        username,
        account_id,
        v,
        resolved,
        rendered,
        scheduled_deletion_days,
        idem_key,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The status insert, callable on an existing transaction (the
/// scheduled-publish path bundles media moves + scheduled-row cleanup).
/// `idem_key` back-fills the idempotency record inside the transaction.
#[allow(clippy::too_many_arguments)]
pub async fn insert_status_tx(
    ctx: &PostContext,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    username: &str,
    account_id: i64,
    v: &ValidatedStatus,
    resolved: &[mentions::ResolvedMention],
    rendered: &render::RenderedPost,
    scheduled_deletion_days: i32,
    idem_key: Option<&str>,
) -> Result<i64> {
    let parent_account: Option<i64> = match v.in_reply_to_id {
        Some(pid) => {
            sqlx::query_scalar("SELECT account_id FROM statuses WHERE id = $1")
                .bind(pid)
                .fetch_optional(&mut **tx)
                .await?
        }
        None => None,
    };
    let status_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, content_html, content_type, language, \
             visibility, spoiler_text, sensitive, local, in_reply_to_id, \
             in_reply_to_account_id, scheduled_deletion_at, quote_of_id) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,true,$9,$10, \
                 CASE WHEN $11::int > 0 THEN now() + $11::int * interval '1 day' ELSE NULL END, $12) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&v.status)
    .bind(&rendered.html)
    .bind(&v.content_type)
    .bind(&v.language)
    .bind(&v.visibility)
    .bind(&v.spoiler_text)
    .bind(v.sensitive)
    .bind(v.in_reply_to_id)
    .bind(parent_account)
    .bind(scheduled_deletion_days)
    .bind(v.quote_of_id)
    .fetch_one(&mut **tx)
    .await?;

    let (uri, url) = ctx.status_urls(username, status_id);
    sqlx::query("UPDATE statuses SET uri = $2, url = $3 WHERE id = $1")
        .bind(status_id)
        .bind(&uri)
        .bind(&url)
        .execute(&mut **tx)
        .await?;

    sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
        .bind(account_id)
        .execute(&mut **tx)
        .await?;
    if let Some(parent) = v.in_reply_to_id {
        sqlx::query("UPDATE statuses SET replies_count = replies_count + 1 WHERE id = $1")
            .bind(parent)
            .execute(&mut **tx)
            .await?;
    }
    if let Some(quoted) = v.quote_of_id {
        sqlx::query("UPDATE statuses SET quotes_count = quotes_count + 1 WHERE id = $1")
            .bind(quoted)
            .execute(&mut **tx)
            .await?;
    }
    for media_id in v.media_ids.as_deref().unwrap_or_default() {
        // The reservation was checked in `validate`, but a concurrent
        // post could have claimed it since — fail rather than silently
        // producing a status that claims media it doesn't own.
        let claimed = sqlx::query(
            "UPDATE media_attachments SET status_id = $1, updated_at = now() \
             WHERE id = $2 AND account_id = $3 AND status_id IS NULL",
        )
        .bind(status_id)
        .bind(media_id)
        .bind(account_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if claimed == 0 {
            return Err(Error::Validation(format!(
                "media {media_id} is no longer attachable"
            )));
        }
    }
    if let Some(poll) = &v.poll {
        let poll_id = polls::create(&mut *tx, status_id, poll).await?;
        sqlx::query("UPDATE statuses SET poll_id = $2 WHERE id = $1")
            .bind(status_id)
            .bind(poll_id)
            .execute(&mut **tx)
            .await?;
    }
    mentions::persist(&mut *tx, status_id, resolved).await?;
    for tag in &rendered.hashtags {
        let tag_id: i64 = sqlx::query_scalar(
            "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id",
        )
        .bind(tag)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO status_tags (status_id, tag_id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(status_id)
        .bind(tag_id)
        .execute(&mut **tx)
        .await?;
    }
    if let Some(key) = idem_key {
        idem_complete(&mut *tx, account_id, key, "status", status_id).await?;
    }
    Ok(status_id)
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// `POST /api/v1/statuses` — the full pipeline; returns the entity to
/// serve (Status or ScheduledStatus).
pub async fn create_status(
    ctx: &PostContext,
    auth: &AuthContext,
    params: &StatusParams,
    idempotency_key: Option<&str>,
) -> Result<CreateOutcome> {
    let validated = validate(ctx, auth, params, None).await?;
    let scheduled = params.scheduled_at.is_some_and(|t| t > Utc::now());
    let fp = fingerprint(&validated, scheduled);

    if let Some(key) = idempotency_key.filter(|k| !k.is_empty()) {
        match idem_begin(ctx, auth.account_id, key, &fp).await? {
            IdemReservation::Replay {
                target_type,
                target_id,
            } => {
                return replay(ctx, &target_type, target_id).await;
            }
            IdemReservation::InFlight => {
                return Err(Error::Conflict(
                    "a request with this Idempotency-Key is in flight".to_string(),
                ));
            }
            IdemReservation::Mismatch => {
                return Err(Error::Validation(
                    "Idempotency-Key was used with a different payload".to_string(),
                ));
            }
            IdemReservation::Reserved => {}
        }
        match create_inner(ctx, auth, &validated, params.scheduled_at, Some(key)).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                idem_release(ctx, auth.account_id, key).await;
                Err(e)
            }
        }
    } else {
        create_inner(ctx, auth, &validated, params.scheduled_at, None).await
    }
}

/// Replay a completed idempotent create by re-rendering the stored
/// target's entity.
async fn replay(ctx: &PostContext, target_type: &str, target_id: i64) -> Result<CreateOutcome> {
    match target_type {
        "scheduled_status" => {
            let row = scheduled::load(ctx, target_id).await?;
            match row {
                Some(r) => Ok(CreateOutcome::Scheduled(
                    entity::scheduled_status_entity(ctx, &r).await?,
                )),
                // The scheduled row was consumed by publishing since —
                // replay as a gone resource.
                None => Err(Error::NotFound("scheduled status".into())),
            }
        }
        _ => match super::load(&ctx.db, target_id).await? {
            Some(status) => Ok(CreateOutcome::Published(
                entity::status_entity(ctx, &status, None).await?,
            )),
            None => Err(Error::NotFound("status".into())),
        },
    }
}

/// The shared create path once validation passed: schedule or
/// publish. `idem_key` completes the idempotency record inside the
/// create transaction so commit and back-fill are atomic.
async fn create_inner(
    ctx: &PostContext,
    auth: &AuthContext,
    validated: &ValidatedStatus,
    scheduled_at: Option<DateTime<Utc>>,
    idem_key: Option<&str>,
) -> Result<CreateOutcome> {
    if let Some(at) = scheduled_at {
        if at > Utc::now() {
            let row =
                scheduled::create_scheduled(ctx, auth.account_id, validated, at, idem_key).await?;
            return Ok(CreateOutcome::Scheduled(
                entity::scheduled_status_entity(ctx, &row).await?,
            ));
        }
    }
    let status = publish(ctx, auth.account_id, validated, idem_key).await?;
    Ok(CreateOutcome::Published(
        entity::status_entity(ctx, &status, Some(auth.account_id)).await?,
    ))
}

/// Resolve mentions, render, persist and fan out — shared by the API
/// create and the scheduled-publish job.
pub async fn publish(
    ctx: &PostContext,
    account_id: i64,
    validated: &ValidatedStatus,
    idem_key: Option<&str>,
) -> Result<StatusRow> {
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&ctx.db)
        .await?;
    let parent = match validated.in_reply_to_id {
        Some(id) => super::load(&ctx.db, id).await?,
        None => None,
    };
    let resolved = mentions::resolve(ctx, &validated.status, account_id, parent.as_ref()).await?;
    let links: Vec<render::MentionLink> = resolved.iter().filter_map(|m| m.link()).collect();
    let rendered = render::render(
        &validated.status,
        &validated.content_type,
        &links,
        &ctx.base_url(),
        &ctx.config,
    );
    // Quotes get the Pleroma `RE:` suffix appended to the rendered
    // HTML (source stays verbatim). A target that vanished since
    // validation drops the quote.
    let mut rendered = rendered;
    let mut effective = validated.clone();
    let quote_target = crate::interactions::quote::prepare_quote(
        ctx,
        &mut effective.quote_of_id,
        &mut rendered.html,
    )
    .await?;
    let deletion_days = scheduled_deletion_days(ctx, account_id).await;
    let status_id = insert_status(
        ctx,
        &username,
        account_id,
        &effective,
        &resolved,
        &rendered,
        deletion_days,
        idem_key,
    )
    .await?;
    let status = super::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after insert".to_string()))?;

    // Fan-out hooks (best-effort — the row is already durable).
    super::preview::enqueue(
        ctx,
        &status,
        validated.media_ids.as_deref().unwrap_or_default(),
    )
    .await;
    crate::timelines::fanout::enqueue(&ctx.dispatcher, status.id).await;
    crate::timelines::stream::publish_update(ctx, &status).await;
    crate::notifications::emit::emit_for_status(&ctx.db, &ctx.dispatcher, &status).await;
    if let Some(ap) = &ctx.ap {
        if let Err(e) = crate::activitypub::posts::publish_create(ap, ctx, &status, &resolved).await
        {
            tracing::error!(status_id, error = %e, "AP Create publish failed");
        }
    }
    if let Some(target) = &quote_target {
        crate::interactions::quote::finish_quote(ctx, ctx.ap.as_ref(), &status, target).await;
    }
    Ok(status)
}

/// `users.scheduled_deletion_days` for the posting account (0/NULL =
/// never).
async fn scheduled_deletion_days(ctx: &PostContext, account_id: i64) -> i32 {
    sqlx::query_scalar::<_, Option<i32>>(
        "SELECT scheduled_deletion_days FROM users WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(&ctx.db)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Edit / delete
// ---------------------------------------------------------------------------

/// `PUT /api/v1/statuses/{id}` — owner-only edit: re-validate,
/// re-render, re-sync mentions/tags/media, stamp `edited_at`, publish
/// `Update`. Poll edits only while the poll has no votes.
pub async fn edit_status(
    ctx: &PostContext,
    auth: &AuthContext,
    status_id: i64,
    params: &StatusParams,
) -> Result<StatusRow> {
    let status = super::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if status.account_id != auth.account_id {
        return Err(Error::NotFound("status".into()));
    }
    if !status.local {
        return Err(Error::Forbidden("remote statuses cannot be edited"));
    }
    let validated = validate(ctx, auth, params, Some(status_id)).await?;

    // A status carries a poll or media, never both: media supplied on
    // edit must not meet a kept poll (a `poll` supplied alongside
    // media is already rejected by `validate`).
    if status.poll_id.is_some()
        && validated
            .media_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty())
    {
        return Err(Error::Validation(
            "a status is either poll or media, not both".to_string(),
        ));
    }

    // Poll edits only while voteless: a supplied `poll` replaces the
    // existing one; omitting it leaves the current poll untouched.
    if validated.poll.is_some() {
        if let Some(poll_id) = status.poll_id {
            let votes: i64 =
                sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE poll_id = $1")
                    .bind(poll_id)
                    .fetch_one(&ctx.db)
                    .await?;
            if votes > 0 {
                return Err(Error::Validation(
                    "a poll cannot be edited after votes were cast".to_string(),
                ));
            }
        }
    }

    let parent = match validated.in_reply_to_id {
        Some(id) => super::load(&ctx.db, id).await?,
        None => None,
    };
    let resolved =
        mentions::resolve(ctx, &validated.status, auth.account_id, parent.as_ref()).await?;
    let links: Vec<render::MentionLink> = resolved.iter().filter_map(|m| m.link()).collect();
    let rendered = render::render(
        &validated.status,
        &validated.content_type,
        &links,
        &ctx.base_url(),
        &ctx.config,
    );

    let mut tx = ctx.db.begin().await?;
    sqlx::query(
        "UPDATE statuses SET content = $2, content_html = $3, content_type = $4, \
             language = $5, sensitive = $6, spoiler_text = $7, visibility = $8, \
             edited_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(status_id)
    .bind(&validated.status)
    .bind(&rendered.html)
    .bind(&validated.content_type)
    .bind(&validated.language)
    .bind(validated.sensitive)
    .bind(&validated.spoiler_text)
    .bind(&validated.visibility)
    .execute(&mut *tx)
    .await?;
    // `media_ids` supplied (even empty) replaces the attachment set;
    // omitted keeps it (Mastodon edit semantics).
    if let Some(media_ids) = &validated.media_ids {
        sqlx::query("UPDATE media_attachments SET status_id = NULL WHERE status_id = $1")
            .bind(status_id)
            .execute(&mut *tx)
            .await?;
        for media_id in media_ids {
            let claimed = sqlx::query(
                "UPDATE media_attachments SET status_id = $1, updated_at = now() \
                 WHERE id = $2 AND account_id = $3 AND status_id IS NULL",
            )
            .bind(status_id)
            .bind(media_id)
            .bind(auth.account_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if claimed == 0 {
                return Err(Error::Validation(format!(
                    "media {media_id} is no longer attachable"
                )));
            }
        }
    }
    if let Some(poll) = &validated.poll {
        if let Some(old) = status.poll_id {
            // `poll_options`/`poll_votes` cascade; `statuses.poll_id`
            // SET NULL — re-pointed right after.
            sqlx::query("DELETE FROM polls WHERE id = $1")
                .bind(old)
                .execute(&mut *tx)
                .await?;
        }
        let poll_id = polls::create(&mut tx, status_id, poll).await?;
        sqlx::query("UPDATE statuses SET poll_id = $2 WHERE id = $1")
            .bind(status_id)
            .bind(poll_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM status_mentions WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    mentions::persist(&mut tx, status_id, &resolved).await?;
    sqlx::query("DELETE FROM status_tags WHERE status_id = $1")
        .bind(status_id)
        .execute(&mut *tx)
        .await?;
    for tag in &rendered.hashtags {
        let tag_id: i64 = sqlx::query_scalar(
            "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) DO UPDATE SET name = EXCLUDED.name \
             RETURNING id",
        )
        .bind(tag)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO status_tags (status_id, tag_id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(status_id)
        .bind(tag_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let status = super::load(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after edit".to_string()))?;
    if let Some(ap) = &ctx.ap {
        if let Err(e) = crate::activitypub::posts::publish_update(ap, ctx, &status, &resolved).await
        {
            tracing::error!(status_id, error = %e, "AP Update publish failed");
        }
        crate::activitypub::posts::invalidate_object(ap, status_id);
    }
    // Stream the `update` event — subscribed timelines replace the row
    // in place, so an edit reflects without a reload.
    crate::timelines::stream::publish_update(ctx, &status).await;
    // When the edit kept the attachments (`media_ids` omitted), read
    // them back so the preview check still sees the effective set.
    let effective_media: Vec<i64> = match &validated.media_ids {
        Some(ids) => ids.clone(),
        None => {
            sqlx::query_scalar("SELECT id FROM media_attachments WHERE status_id = $1")
                .bind(status_id)
                .fetch_all(&ctx.db)
                .await?
        }
    };
    super::preview::enqueue(ctx, &status, &effective_media).await;
    Ok(status)
}

/// `DELETE /api/v1/statuses/{id}` — owner-only tombstone: clear the
/// body, detach media objects, publish `Delete`, invalidate the
/// object cache. Idempotent (a tombstoned row is already gone).
pub async fn delete_status(
    ctx: &PostContext,
    account_id: i64,
    status_id: i64,
) -> Result<StatusRow> {
    let status = super::load_any(&ctx.db, status_id)
        .await?
        .ok_or_else(|| Error::NotFound("status".into()))?;
    if status.account_id != account_id {
        return Err(Error::NotFound("status".into()));
    }
    if status.tombstone {
        return Err(Error::NotFound("status".into()));
    }
    // Deleting a boost row is an unreblog — `Undo(Announce)`, not the
    // `Delete` envelope a normal status gets.
    if status.reblog_of_id.is_some() {
        return crate::interactions::boost::unreblog_row(ctx, &status).await;
    }
    tombstone(ctx, &status).await
}

/// The tombstone path shared by `DELETE` and the `StatusDeletion`
/// sweep: scrub content + media objects, publish `Delete`, invalidate.
/// Storage objects go first; a failed delete detaches the row
/// (`status_id = NULL`) so the orphan cleanup reclaims it later
/// instead of leaking an object no row points at.
pub async fn tombstone(ctx: &PostContext, status: &StatusRow) -> Result<StatusRow> {
    let media: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, file_path FROM media_attachments WHERE status_id = $1")
            .bind(status.id)
            .fetch_all(&ctx.db)
            .await?;
    for (media_id, key) in &media {
        if let Some(key) = key {
            if let Err(e) = ctx.media.delete(key).await {
                tracing::warn!(key, error = %e, "media object delete failed; detaching row");
                sqlx::query("UPDATE media_attachments SET status_id = NULL WHERE id = $1")
                    .bind(media_id)
                    .execute(&ctx.db)
                    .await?;
                continue;
            }
        }
        sqlx::query("DELETE FROM media_attachments WHERE id = $1")
            .bind(media_id)
            .execute(&ctx.db)
            .await?;
    }
    sqlx::query(
        "UPDATE statuses SET content = '', content_html = '', spoiler_text = NULL, \
             sensitive = false, tombstone = true, updated_at = now() WHERE id = $1",
    )
    .bind(status.id)
    .execute(&ctx.db)
    .await?;
    // The deletion releases the author's post counter; a deleted reply
    // releases its parent's counter; same for a deleted quote post's
    // target.
    sqlx::query(
        "UPDATE accounts SET statuses_count = GREATEST(statuses_count - 1, 0) WHERE id = $1",
    )
    .bind(status.account_id)
    .execute(&ctx.db)
    .await?;
    if let Some(parent) = status.in_reply_to_id {
        sqlx::query(
            "UPDATE statuses SET replies_count = GREATEST(replies_count - 1, 0) WHERE id = $1",
        )
        .bind(parent)
        .execute(&ctx.db)
        .await?;
    }
    if let Some(quoted) = status.quote_of_id {
        sqlx::query(
            "UPDATE statuses SET quotes_count = GREATEST(quotes_count - 1, 0) WHERE id = $1",
        )
        .bind(quoted)
        .execute(&ctx.db)
        .await?;
    }
    if let Some(ap) = &ctx.ap {
        if status.local {
            if let Err(e) = crate::activitypub::posts::publish_delete(ap, ctx, status).await {
                tracing::error!(status_id = status.id, error = %e, "AP Delete publish failed");
            }
        }
        crate::activitypub::posts::invalidate_object(ap, status.id);
    }
    crate::timelines::fanout::remove_quiet(
        &ctx.db,
        &ctx.redis,
        &ctx.config.timelines,
        status.id,
        status.account_id,
    )
    .await;
    crate::timelines::stream::publish_delete(&ctx.db, &ctx.bus, status).await;
    super::load_any(&ctx.db, status.id)
        .await?
        .ok_or_else(|| Error::Internal("status missing after delete".to_string()))
}
