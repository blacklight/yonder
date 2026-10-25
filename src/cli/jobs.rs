//! `yonder run-job` — enqueue a background job for the worker
//! (default), or execute its production handler in-process with
//! `--inline`. Inline runs share the worker's [`build_registry`]
//! wiring but bypass retry and dead-letter accounting.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use crate::{jobs::JobKind, Error, Result};

use super::{build_registry, context, output::Output};

/// Resolve `name` to a [`JobKind`] by its wire name,
/// case-insensitively. Unknown kinds list the valid names.
pub fn resolve_kind(name: &str) -> Result<JobKind> {
    JobKind::ALL
        .iter()
        .copied()
        .find(|k| k.as_str().eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let kinds = JobKind::ALL
                .iter()
                .map(JobKind::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            Error::NotFound(format!("job kind {name:?} (valid kinds: {kinds})"))
        })
}

pub async fn run_job(
    path: Option<&Path>,
    kind: &str,
    payload: Option<&str>,
    inline: bool,
    delay: Option<u64>,
    out: &Output,
) -> Result<u8> {
    let kind = resolve_kind(kind)?;
    let payload: Value = match payload {
        None => json!({}),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| Error::Validation(format!("--payload is not valid JSON: {e}")))?,
    };
    if inline && delay.is_some() {
        return Err(Error::Validation(
            "--delay applies to enqueued jobs only".to_string(),
        ));
    }

    if inline {
        run_inline(path, kind, &payload, out).await
    } else {
        enqueue(path, kind, payload, delay, out).await
    }
}

async fn enqueue(
    path: Option<&Path>,
    kind: JobKind,
    payload: Value,
    delay: Option<u64>,
    out: &Output,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let id = match delay {
        None => ctx.state.dispatcher.enqueue(kind, payload).await?,
        Some(secs) => {
            // `enqueue_delayed` adds the current epoch to the delay
            // itself — reject delays that would overflow the u64
            // millisecond timestamp rather than wrap.
            let delay = Duration::from_secs(secs);
            let delay_ms = u64::try_from(delay.as_millis())
                .map_err(|_| Error::Validation("--delay is too large".to_string()))?;
            let now_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now_ms.checked_add(delay_ms).ok_or_else(|| {
                Error::Validation("--delay is too large (timestamp overflow)".to_string())
            })?;
            ctx.state
                .dispatcher
                .enqueue_delayed(kind, payload, delay)
                .await?
        }
    };
    let queue = kind.queue().as_str();
    if out.json_mode() {
        out.emit_json(&json!({"id": id, "queue": queue, "inline": false}))?;
    } else {
        out.emit_human(&format!("enqueued {id} on queue {queue}"))?;
    }
    Ok(0)
}

async fn run_inline(
    path: Option<&Path>,
    kind: JobKind,
    payload: &Value,
    out: &Output,
) -> Result<u8> {
    let ctx = context::load_runtime(path, true).await?;
    let registry = build_registry(ctx.ap.as_ref());
    let handler = registry
        .handler(kind)
        .ok_or_else(|| Error::NotFound(format!("no handler registered for {}", kind.as_str())))?;
    let job_ctx = crate::jobs::JobContext {
        db: ctx.pool.clone(),
        redis: ctx.redis.clone(),
        config: Arc::new(ctx.config.clone()),
        media: crate::media::storage::MediaStore::from_config(&ctx.config),
    };
    out.warn("running job inline (bypasses worker retry/dead-letter accounting)");
    handler.run(&job_ctx, payload).await?;
    if out.json_mode() {
        out.emit_json(&json!({"inline": true, "ok": true}))?;
    } else {
        out.emit_human("job completed")?;
    }
    Ok(0)
}
