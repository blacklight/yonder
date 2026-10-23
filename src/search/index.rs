//! Pluggable search backend. `SearchIndex` is the seam an
//! Elasticsearch/OpenSearch implementation plugs into
//! (`search.elasticsearch_url`); [`PostgresIndex`] is the default and
//! only shipping backend — it delegates to the trigram queries and
//! needs no indexing hooks (the DB *is* the index).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sqlx::PgPool;

use crate::config::SearchConfig;
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
    /// Status page, newest-first.
    async fn search_statuses(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
    ) -> Result<Vec<StatusRow>>;
    /// Tag page for `tag` — exact match, or prefix when `prefix` is
    /// set (autocomplete).
    async fn search_hashtags(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        tag: &str,
        prefix: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TagHit>>;
    /// (Re)index a status — no-op for Postgres.
    async fn index_status(&self, status_id: i64) -> Result<()>;
    /// (Re)index an account — no-op for Postgres.
    async fn index_account(&self, account_id: i64) -> Result<()>;
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
    ) -> Result<Vec<StatusRow>> {
        statuses::search(db, viewer, params, kind).await
    }

    async fn search_hashtags(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        tag: &str,
        prefix: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TagHit>> {
        hashtags::search(db, viewer, tag, prefix, limit, offset).await
    }

    async fn index_status(&self, _status_id: i64) -> Result<()> {
        Ok(())
    }

    async fn index_account(&self, _account_id: i64) -> Result<()> {
        Ok(())
    }

    async fn remove_status(&self, _status_id: i64) -> Result<()> {
        Ok(())
    }

    async fn remove_account(&self, _account_id: i64) -> Result<()> {
        Ok(())
    }
}

/// Pick the configured backend. `elasticsearch_url` is accepted but
/// the ES/OS implementation is not shipped — warn once per process
/// and fall back to Postgres.
pub fn build_index(config: &SearchConfig) -> Arc<dyn SearchIndex> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if config.elasticsearch_url.is_some() && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "search.elasticsearch_url is configured but the \
             Elasticsearch/OpenSearch backend is not implemented; \
             falling back to Postgres"
        );
    }
    Arc::new(PostgresIndex)
}
