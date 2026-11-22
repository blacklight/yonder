//! Search-index job handlers — incremental sync and bulk backfill for
//! the Elasticsearch/OpenSearch backend (`search.elasticsearch_url`).
//!
//! - `SearchIndexSync` `{"entity": "status"|"account", "id": N,
//!   "op": "index"|"remove"}` — enqueued by the write paths that touch
//!   search-visible text. A no-op under `PostgresIndex`.
//! - `SearchIndexBackfill` `{"entity": "statuses"|"accounts"|"all",
//!   "after_id": N, "batch_size": N}` — cursor-paginates the table in
//!   id order, bulk-indexes each batch and re-enqueues itself until a
//!   short batch ends the walk. Trigger by hand:
//!   `yonder run-job search_index_backfill --payload '{"entity":"all"}'`.
//!
//! Both handlers build the index lazily from `ctx.config.search` —
//! `JobContext` carries no `AppState`, and the worker's config is a
//! startup snapshot anyway (same lifetime as `AppState.search_index`).

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::OnceCell;

use crate::jobs::{JobContext, JobHandler, JobKind};
use crate::Result;

use super::index::{self, SearchIndex};

/// Default backfill batch — each iteration is one `_bulk` request per
/// entity plus a cursor query.
const DEFAULT_BATCH: i64 = 500;
/// Hard cap so a fat-fingered payload can't request unbounded pages.
const MAX_BATCH: i64 = 5000;

/// Lazily-built index shared by both handlers (one per registry, i.e.
/// per process).
struct IndexCell {
    inner: OnceCell<Arc<dyn SearchIndex>>,
}

impl IndexCell {
    fn new() -> Self {
        IndexCell {
            inner: OnceCell::new(),
        }
    }

    async fn get(&self, ctx: &JobContext) -> Arc<dyn SearchIndex> {
        self.inner
            .get_or_init(|| async { index::build_index(&ctx.config.search) })
            .await
            .clone()
    }
}

/// `JobKind::SearchIndexSync` — one document upsert or delete.
pub struct SearchIndexSyncJob {
    index: IndexCell,
}

impl SearchIndexSyncJob {
    pub fn new() -> Self {
        SearchIndexSyncJob {
            index: IndexCell::new(),
        }
    }
}

impl Default for SearchIndexSyncJob {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl JobHandler for SearchIndexSyncJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let entity = payload.get("entity").and_then(Value::as_str).unwrap_or("");
        let id = payload.get("id").and_then(Value::as_i64).unwrap_or(0);
        let op = payload.get("op").and_then(Value::as_str).unwrap_or("index");
        if id <= 0 {
            tracing::warn!(payload = %payload, "search index sync: missing id");
            return Ok(());
        }
        let index = self.index.get(ctx).await;
        match (entity, op) {
            ("status", "remove") => index.remove_status(id).await,
            ("account", "remove") => index.remove_account(id).await,
            ("status", _) => index.index_status(&ctx.db, id).await,
            ("account", _) => index.index_account(&ctx.db, id).await,
            _ => {
                tracing::warn!(entity, op, "search index sync: unknown entity/op");
                Ok(())
            }
        }
    }
}

/// `JobKind::SearchIndexBackfill` — paged bulk reindex.
pub struct SearchIndexBackfillJob {
    index: IndexCell,
}

impl SearchIndexBackfillJob {
    pub fn new() -> Self {
        SearchIndexBackfillJob {
            index: IndexCell::new(),
        }
    }
}

impl Default for SearchIndexBackfillJob {
    fn default() -> Self {
        Self::new()
    }
}

/// One backfill page for `entity`: ids `> after_id`, bulk-index them,
/// return `(last_id, full)` — `full` means another page may exist.
async fn backfill_page(
    ctx: &JobContext,
    index: &Arc<dyn SearchIndex>,
    entity: &str,
    after_id: i64,
    batch: i64,
) -> Result<(i64, bool)> {
    let table = match entity {
        "statuses" => "statuses",
        "accounts" => "accounts",
        _ => return Ok((after_id, false)),
    };
    let ids: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id > $1 ORDER BY id LIMIT $2"
    ))
    .bind(after_id)
    .bind(batch)
    .fetch_all(&ctx.db)
    .await?;
    if ids.is_empty() {
        return Ok((after_id, false));
    }
    match entity {
        "statuses" => index.index_statuses(&ctx.db, &ids).await?,
        _ => index.index_accounts(&ctx.db, &ids).await?,
    }
    let last = *ids.last().unwrap_or(&after_id);
    Ok((last, ids.len() as i64 == batch))
}

#[async_trait::async_trait]
impl JobHandler for SearchIndexBackfillJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let entity = payload
            .get("entity")
            .and_then(Value::as_str)
            .unwrap_or("all");
        // `"all"` fans out once at the root; continuations always carry
        // a concrete entity.
        if entity == "all" {
            let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
            for e in ["statuses", "accounts"] {
                dispatcher
                    .enqueue(
                        JobKind::SearchIndexBackfill,
                        serde_json::json!({"entity": e, "after_id": 0}),
                    )
                    .await?;
            }
            return Ok(());
        }
        let after_id = payload.get("after_id").and_then(Value::as_i64).unwrap_or(0);
        let batch = payload
            .get("batch_size")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_BATCH)
            .clamp(1, MAX_BATCH);
        let index = self.index.get(ctx).await;
        let (last, full) = backfill_page(ctx, &index, entity, after_id, batch).await?;
        if full {
            let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
            dispatcher
                .enqueue(
                    JobKind::SearchIndexBackfill,
                    serde_json::json!({
                        "entity": entity,
                        "after_id": last,
                        "batch_size": batch,
                    }),
                )
                .await?;
        } else {
            tracing::info!(entity, after_id = last, "search index backfill complete");
        }
        Ok(())
    }
}

/// Register the search-index handlers (called by
/// [`crate::cli::build_registry`] via [`super::register_jobs`]).
pub fn register(registry: &mut crate::jobs::Registry) {
    registry.register(JobKind::SearchIndexSync, SearchIndexSyncJob::new());
    registry.register(JobKind::SearchIndexBackfill, SearchIndexBackfillJob::new());
}
