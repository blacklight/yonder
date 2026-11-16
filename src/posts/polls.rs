//! Polls — create validation, transactional voting, results (epic 12).
//!
//! Voting serializes on the `polls` row (`SELECT … FOR UPDATE`): the
//! whole request — every `poll_votes` insert and every
//! `poll_options.votes_count` bump — commits or rolls back atomically,
//! so a partial-duplicate multi-choice request leaves no trace. The
//! duplicate contract is Mastodon-compatible: re-voting is a 422, not
//! an idempotent success.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use sqlx::Row;

use crate::{Error, Result};

use super::PostContext;

/// A poll as submitted inside a status create.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PollParams {
    /// Option titles (2..=`polls.max_options`).
    pub options: Vec<String>,
    /// Seconds until the poll closes.
    pub expires_in: u64,
    /// Allow several choices per voter.
    pub multiple: bool,
    /// Hide totals until expiry.
    pub hide_totals: bool,
}

/// Validate a poll against `polls.*` config — option count/chars and
/// duration bounds. Options are trimmed; empties are dropped by the
/// caller-facing parser before this runs.
pub fn validate(ctx: &PostContext, poll: &PollParams) -> Result<()> {
    let cfg = &ctx.config.polls;
    if poll.options.len() < 2 || poll.options.len() > cfg.max_options as usize {
        return Err(Error::Validation(format!(
            "poll requires 2..={} options",
            cfg.max_options
        )));
    }
    for option in &poll.options {
        if option.is_empty() || option.chars().count() > cfg.max_option_chars as usize {
            return Err(Error::Validation(format!(
                "poll options must be 1..={} characters",
                cfg.max_option_chars
            )));
        }
    }
    if poll.expires_in < cfg.min_duration_seconds || poll.expires_in > cfg.max_duration_seconds {
        return Err(Error::Validation(format!(
            "poll expires_in must be {}..={} seconds",
            cfg.min_duration_seconds, cfg.max_duration_seconds
        )));
    }
    Ok(())
}

/// Insert `polls` + `poll_options` inside the status transaction;
/// returns the new `polls.id`.
pub async fn create(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    poll: &PollParams,
) -> Result<i64> {
    let poll_id: i64 = sqlx::query_scalar(
        "INSERT INTO polls (status_id, expires_at, multiple, hide_totals) \
         VALUES ($1, now() + $2::bigint * interval '1 second', $3, $4) RETURNING id",
    )
    .bind(status_id)
    .bind(poll.expires_in as i64)
    .bind(poll.multiple)
    .bind(poll.hide_totals)
    .fetch_one(&mut *conn)
    .await?;
    for (idx, option) in poll.options.iter().enumerate() {
        sqlx::query("INSERT INTO poll_options (poll_id, idx, title) VALUES ($1, $2, $3)")
            .bind(poll_id)
            .bind(idx as i32)
            .bind(option)
            .execute(&mut *conn)
            .await?;
    }
    Ok(poll_id)
}

/// The status owning `poll_id` (`None` hides the poll's existence).
async fn owning_status(ctx: &PostContext, poll_id: i64) -> Result<Option<super::StatusRow>> {
    let status_id: Option<i64> = sqlx::query_scalar("SELECT status_id FROM polls WHERE id = $1")
        .bind(poll_id)
        .fetch_optional(&ctx.db)
        .await?;
    match status_id {
        Some(id) => super::load(&ctx.db, id).await,
        None => Ok(None),
    }
}

/// `can_see` gate for poll endpoints — the poll is only as visible as
/// the status carrying it.
pub async fn status_visible(ctx: &PostContext, poll_id: i64, viewer: Option<i64>) -> Result<bool> {
    match owning_status(ctx, poll_id).await? {
        Some(status) => super::can_see(&ctx.db, &status, viewer).await,
        None => Ok(false),
    }
}

/// Self-heal for poll reads on remote statuses — enqueue a tally
/// refresh (deduped) so the next render sees votes the origin
/// collected since ingest, the lazy counterpart of the
/// likes/shares/quotes counter refresh. Local polls and
/// federation-off contexts are no-ops; failures only log.
pub async fn refresh_remote(ctx: &PostContext, poll_id: i64) {
    let Some(ap) = &ctx.ap else {
        return;
    };
    let Ok(Some(status)) = owning_status(ctx, poll_id).await else {
        return;
    };
    if status.local {
        return;
    }
    if let Some(uri) = &status.uri {
        if let Err(e) = super::remote::enqueue_poll_refresh(&ap.core, uri).await {
            tracing::warn!(poll_id, error = %e, "poll tally refresh enqueue failed");
        }
    }
}

/// `POST /api/v1/polls/{id}/votes` — dedupe the submitted choices,
/// then vote atomically under the poll row lock: poll exists and is
/// unexpired, voter ≠ author, arity matches `multiple`, every index in
/// bounds. Any failure or duplicate vote rolls the request back.
pub async fn vote(
    ctx: &PostContext,
    poll_id: i64,
    voter_account_id: i64,
    choices: &[i64],
) -> Result<()> {
    // Dedupe first — duplicate indexes in one request are a 422 (the
    // request is malformed, not double-counted).
    let deduped: BTreeSet<i64> = choices.iter().copied().collect();
    if deduped.len() != choices.len() || choices.is_empty() {
        return Err(Error::Validation(
            "choices contains duplicate indexes".to_string(),
        ));
    }
    // Audience gate — a poll is only as visible as its status.
    if !status_visible(ctx, poll_id, Some(voter_account_id)).await? {
        return Err(Error::NotFound("poll".into()));
    }

    let mut tx = ctx.db.begin().await?;
    let poll = sqlx::query(
        "SELECT p.id, p.expires_at, p.multiple, s.account_id AS author_id, \
                (SELECT count(*) FROM poll_options WHERE poll_id = p.id) AS option_count \
         FROM polls p JOIN statuses s ON s.poll_id = p.id \
         WHERE p.id = $1 FOR UPDATE OF p",
    )
    .bind(poll_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::NotFound("poll".into()))?;

    let expires_at: Option<chrono::DateTime<chrono::Utc>> = poll.get("expires_at");
    if expires_at.is_some_and(|e| e <= chrono::Utc::now()) {
        return Err(Error::Validation("poll has expired".to_string()));
    }
    if poll.get::<i64, _>("author_id") == voter_account_id {
        return Err(Error::Validation(
            "cannot vote on your own poll".to_string(),
        ));
    }
    let multiple: bool = poll.get("multiple");
    if !multiple && choices.len() != 1 {
        return Err(Error::Validation(
            "single-choice poll accepts exactly one choice".to_string(),
        ));
    }
    let option_count: i64 = poll.get("option_count");
    if choices.iter().any(|c| *c < 0 || *c >= option_count) {
        return Err(Error::Validation("choice index out of bounds".to_string()));
    }

    // Already-voted-anything is a 422 — distinct new options on a
    // second request are not merged votes.
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2)",
    )
    .bind(poll_id)
    .bind(voter_account_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        return Err(Error::Validation("already voted on this poll".to_string()));
    }

    for choice in choices {
        sqlx::query("INSERT INTO poll_votes (poll_id, account_id, option_idx) VALUES ($1, $2, $3)")
            .bind(poll_id)
            .bind(voter_account_id)
            .bind(*choice as i32)
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.is_unique_violation() => {
                    Error::Validation("already voted on this poll".to_string())
                }
                _ => Error::Database(e),
            })?;
        sqlx::query(
            "UPDATE poll_options SET votes_count = votes_count + 1 \
             WHERE poll_id = $1 AND idx = $2",
        )
        .bind(poll_id)
        .bind(*choice as i32)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await.map_err(Error::Database)
}

/// Federated poll answer: a `Create` of a content-less `Note` whose
/// `name` is one option title and whose `inReplyTo` is a stored poll
/// status. Records the ballot in `poll_votes` (tallies bumped in the
/// same transaction) instead of materializing a reply. Returns `true`
/// when the object was consumed as a vote.
pub(crate) async fn ingest_remote_vote(
    core: &crate::activitypub::ApCore,
    actor: &str,
    object: &Value,
) -> bool {
    let has_content = object
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|c| !c.trim().is_empty());
    let (Some(title), Some(parent), false) = (
        object.get("name").and_then(Value::as_str),
        object.get("inReplyTo").and_then(Value::as_str),
        has_content,
    ) else {
        return false;
    };
    match record_remote_vote(core, actor, title, parent).await {
        Ok(consumed) => consumed,
        Err(e) => {
            tracing::warn!(actor, error = %e, "remote poll vote ingest failed");
            false
        }
    }
}

async fn record_remote_vote(
    core: &crate::activitypub::ApCore,
    actor: &str,
    title: &str,
    parent_uri: &str,
) -> Result<bool> {
    let Some(row) = sqlx::query(
        "SELECT p.id AS poll_id, p.expires_at, p.multiple, s.account_id AS author_id, o.idx \
         FROM statuses s JOIN polls p ON p.id = s.poll_id \
         JOIN poll_options o ON o.poll_id = p.id \
         WHERE s.uri = $1 AND o.title = $2",
    )
    .bind(parent_uri)
    .bind(title)
    .fetch_optional(&core.db)
    .await?
    else {
        return Ok(false);
    };
    let poll_id: i64 = row.get("poll_id");
    let idx: i32 = row.get("idx");
    let multiple: bool = row.get("multiple");
    let expires_at: Option<chrono::DateTime<chrono::Utc>> = row.get("expires_at");
    let voter = super::remote::ensure_account(core, actor).await?;
    if row.get::<i64, _>("author_id") == voter
        || expires_at.is_some_and(|e| e <= chrono::Utc::now())
    {
        return Ok(true);
    }

    let mut tx = core.db.begin().await?;
    sqlx::query("SELECT 1 FROM polls WHERE id = $1 FOR UPDATE")
        .bind(poll_id)
        .execute(&mut *tx)
        .await?;
    // Single-choice: one ballot per voter. Multiple-choice: one per option.
    let duplicate: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2 \
         AND ($3 OR option_idx = $4))",
    )
    .bind(poll_id)
    .bind(voter)
    .bind(!multiple)
    .bind(idx)
    .fetch_one(&mut *tx)
    .await?;
    if !duplicate {
        sqlx::query("INSERT INTO poll_votes (poll_id, account_id, option_idx) VALUES ($1, $2, $3)")
            .bind(poll_id)
            .bind(voter)
            .bind(idx)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE poll_options SET votes_count = votes_count + 1 WHERE poll_id = $1 AND idx = $2",
        )
        .bind(poll_id)
        .bind(idx)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// The Mastodon Poll entity for `poll_id` as seen by `viewer`
/// (`voted`/`own_votes` only when authenticated).
pub async fn entity(ctx: &PostContext, poll_id: i64, viewer: Option<i64>) -> Result<Value> {
    let poll = sqlx::query(
        "SELECT expires_at, multiple, hide_totals, created_at FROM polls WHERE id = $1",
    )
    .bind(poll_id)
    .fetch_optional(&ctx.db)
    .await?
    .ok_or_else(|| Error::NotFound("poll".into()))?;

    let options = sqlx::query(
        "SELECT idx, title, votes_count FROM poll_options WHERE poll_id = $1 ORDER BY idx",
    )
    .bind(poll_id)
    .fetch_all(&ctx.db)
    .await?;
    // `votes_count` is the sum of the option tallies — the source of
    // truth. For remote polls the tallies carry the origin's
    // `replies.totalItems` (plus local votes); counting `poll_votes`
    // rows would report 0 (or just the local votes) since remote
    // ballots never materialize rows.
    let votes_count: i64 = options
        .iter()
        .map(|o| o.get::<i32, _>("votes_count") as i64)
        .sum();
    // `voters_count` is Mastodon's `multiple`-only field, and remote
    // Questions carry no distinct-voter total either — emit the
    // locally-known voter count for multiple polls, null otherwise.
    let multiple: bool = poll.get("multiple");
    let voters_count: Value = if multiple {
        Value::from(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(DISTINCT account_id) FROM poll_votes WHERE poll_id = $1",
            )
            .bind(poll_id)
            .fetch_one(&ctx.db)
            .await?,
        )
    } else {
        Value::Null
    };
    let own_votes: Vec<i32> = match viewer {
        Some(account_id) => {
            sqlx::query_scalar(
                "SELECT option_idx FROM poll_votes WHERE poll_id = $1 AND account_id = $2 \
                 ORDER BY option_idx",
            )
            .bind(poll_id)
            .bind(account_id)
            .fetch_all(&ctx.db)
            .await?
        }
        None => Vec::new(),
    };

    let expires_at: Option<chrono::DateTime<chrono::Utc>> = poll.get("expires_at");
    let expired = expires_at.is_some_and(|e| e <= chrono::Utc::now());
    let hide_totals: bool = poll.get("hide_totals");
    let show_totals = !hide_totals || expired;
    Ok(json!({
        "id": poll_id.to_string(),
        "expires_at": expires_at.map(|e| e.to_rfc3339()),
        "expired": expired,
        "multiple": multiple,
        "votes_count": votes_count,
        "voters_count": voters_count,
        "voted": !own_votes.is_empty(),
        "own_votes": own_votes,
        "options": options.iter().map(|o| json!({
            "title": o.get::<String, _>("title"),
            "votes_count": if show_totals { Value::from(o.get::<i32, _>("votes_count")) } else { Value::Null },
        })).collect::<Vec<_>>(),
    }))
}
