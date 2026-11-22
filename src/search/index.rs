//! Pluggable search backend. `SearchIndex` is the seam the
//! Elasticsearch/OpenSearch implementation plugs into
//! (`search.elasticsearch_url`); [`PostgresIndex`] is the default
//! trigram/ILIKE backend — it delegates to the SQL queries and needs
//! no indexing hooks (the DB *is* the index).
//!
//! The ES backend ([`super::elasticsearch::ElasticsearchIndex`]) is a
//! **candidate pre-filter**: it returns matching document ids and the
//! same Postgres clause sets re-apply every visibility/social-graph
//! rule, so only text-bearing fields need index writes — see
//! `features/elasticsearch-search/plan.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;
use sqlx::PgPool;

use crate::config::{Config, SearchConfig};
use crate::jobs::Dispatcher;
use crate::posts::StatusRow;
use crate::Result;

use super::hashtags::TagHit;
use super::params::{QueryKind, SearchParams};
use super::{accounts, hashtags, statuses};

/// Backend interface for indexed search.
#[async_trait::async_trait]
pub trait SearchIndex: Send + Sync {
    /// Ordered `accounts.id` page.
    async fn search_accounts(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
    ) -> Result<Vec<i64>>;
    /// Status page, newest-first. `hide` carries the viewer's epic-20
    /// public-context rules (anonymous callers pass `None`).
    async fn search_statuses(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
        hide: Option<&crate::timelines::filter::HideClause>,
    ) -> Result<Vec<StatusRow>>;
    /// Tag page for `tag` — exact match, or prefix when `prefix` is
    /// set (autocomplete).
    #[allow(clippy::too_many_arguments)]
    async fn search_hashtags(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        tag: &str,
        prefix: bool,
        limit: i64,
        offset: i64,
        hide: Option<&crate::timelines::filter::HideClause>,
    ) -> Result<Vec<TagHit>>;
    /// (Re)index one status from its current row.
    async fn index_status(&self, db: &PgPool, status_id: i64) -> Result<()>;
    /// (Re)index one account from its current row.
    async fn index_account(&self, db: &PgPool, account_id: i64) -> Result<()>;
    /// (Re)index a batch of statuses — default loops
    /// [`Self::index_status`]; ES overrides with `_bulk`.
    async fn index_statuses(&self, db: &PgPool, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.index_status(db, *id).await?;
        }
        Ok(())
    }
    /// (Re)index a batch of accounts — default loops
    /// [`Self::index_account`]; ES overrides with `_bulk`.
    async fn index_accounts(&self, db: &PgPool, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.index_account(db, *id).await?;
        }
        Ok(())
    }
    /// Drop a status from the index — no-op for Postgres.
    async fn remove_status(&self, status_id: i64) -> Result<()>;
    /// Drop an account from the index — no-op for Postgres.
    async fn remove_account(&self, account_id: i64) -> Result<()>;
}

/// Postgres ILIKE/trigram backend — the default path.
pub struct PostgresIndex;

#[async_trait::async_trait]
impl SearchIndex for PostgresIndex {
    async fn search_accounts(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
    ) -> Result<Vec<i64>> {
        accounts::search(db, viewer, params, kind).await
    }

    async fn search_statuses(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
        hide: Option<&crate::timelines::filter::HideClause>,
    ) -> Result<Vec<StatusRow>> {
        statuses::search(db, viewer, params, kind, hide).await
    }

    async fn search_hashtags(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        tag: &str,
        prefix: bool,
        limit: i64,
        offset: i64,
        hide: Option<&crate::timelines::filter::HideClause>,
    ) -> Result<Vec<TagHit>> {
        hashtags::search(db, viewer, tag, prefix, limit, offset, hide).await
    }

    async fn index_status(&self, _db: &PgPool, _status_id: i64) -> Result<()> {
        Ok(())
    }

    async fn index_account(&self, _db: &PgPool, _account_id: i64) -> Result<()> {
        Ok(())
    }

    async fn remove_status(&self, _status_id: i64) -> Result<()> {
        Ok(())
    }

    async fn remove_account(&self, _account_id: i64) -> Result<()> {
        Ok(())
    }
}

/// Pick the configured backend: Elasticsearch/OpenSearch when
/// `elasticsearch_url` is set, Postgres otherwise.
pub fn build_index(config: &SearchConfig) -> Arc<dyn SearchIndex> {
    if let Some(url) = &config.elasticsearch_url {
        return Arc::new(super::elasticsearch::ElasticsearchIndex::new(
            url,
            &config.elasticsearch_index_prefix,
        ));
    }
    Arc::new(PostgresIndex)
}

/// Whether the ES backend is configured — write paths check this
/// before enqueueing `SearchIndexSync` so the Postgres deployment
/// stays free of index-job noise.
pub fn es_enabled(config: &Config) -> bool {
    config.search.elasticsearch_url.is_some()
}

/// Enqueue a `SearchIndexSync` job. No-op when ES is not configured —
/// `PostgresIndex` needs no hooks.
pub async fn enqueue_sync(
    dispatcher: &Dispatcher,
    config: &Config,
    entity: &str,
    id: i64,
    op: &str,
) {
    if !es_enabled(config) {
        return;
    }
    if let Err(e) = dispatcher
        .enqueue(
            crate::jobs::JobKind::SearchIndexSync,
            json!({"entity": entity, "id": id, "op": op}),
        )
        .await
    {
        // Indexing must never fail the write it shadows.
        tracing::warn!(entity, id, op, error = %e, "search index sync enqueue failed");
    }
}

/// [`enqueue_sync`] shorthand for the `index` op.
pub async fn enqueue_index(dispatcher: &Dispatcher, config: &Config, entity: &str, id: i64) {
    enqueue_sync(dispatcher, config, entity, id, "index").await;
}

/// [`enqueue_sync`] shorthand for the `remove` op.
pub async fn enqueue_remove(dispatcher: &Dispatcher, config: &Config, entity: &str, id: i64) {
    enqueue_sync(dispatcher, config, entity, id, "remove").await;
}

static WARNED_UNCONFIGURED: AtomicBool = AtomicBool::new(false);

/// One-shot startup log for the ES backend — kept out of `build_index`
/// because that runs per search request on some paths.
pub fn log_backend(config: &SearchConfig) {
    if config.elasticsearch_url.is_some() {
        tracing::info!(
            url = %config.elasticsearch_url.as_deref().unwrap_or_default(),
            prefix = %config.elasticsearch_index_prefix,
            "elasticsearch search backend enabled"
        );
    } else if !WARNED_UNCONFIGURED.swap(true, Ordering::Relaxed) {
        tracing::debug!("search backend: postgres (trigram)");
    }
}
