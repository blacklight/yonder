//! Elasticsearch/OpenSearch backend — selected by
//! `search.elasticsearch_url`.
//!
//! The index is a **candidate pre-filter**, not the source of truth:
//! ES returns matching document ids and the same Postgres clause sets
//! used by [`super::statuses`]/[`super::accounts`] re-apply every
//! visibility, social-graph and discoverability rule before ordering
//! and pagination. Consequences:
//!
//! - Only text-bearing fields are indexed (`content`, `spoiler_text`
//!   for statuses; `username`, `display_name`, `domain` for accounts) —
//!   visibility flips, suspensions, blocks and opt-outs take effect at
//!   query time with no reindex.
//! - Matching is analyzed (token match, `fuzziness: AUTO`,
//!   `match_phrase_prefix` for autocomplete), not the PG path's true
//!   substring `ILIKE` — mid-token substrings are not recalled. See
//!   `docs/search.md`.
//! - Hard-deleted rows self-heal: a stale id simply matches no PG row.
//!   `remove_*` exists to keep the index lean, not for correctness.
//!
//! Transport is synchronous `ureq` (already a dependency) bridged via
//! `spawn_blocking` — the same pattern as `users::transfer::remote`.
//! Any transport/HTTP failure logs a warning and falls back to the
//! full Postgres query so search survives an index outage.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::posts::StatusRow;
use crate::Result;

use super::hashtags::TagHit;
use super::index::SearchIndex;
use super::params::{is_prefix_mode, QueryKind, SearchParams};
use super::{accounts, hashtags, statuses};

/// Upper bound on ES candidates handed to Postgres per query. Beyond
/// this the result set is approximate (top-N by score); instances
/// small enough to care can raise it — Postgres does the final
/// filtering and ordering either way.
const MAX_CANDIDATES: i64 = 5000;
/// `_bulk` page size for backfill/sync batches.
const BULK_CHUNK: usize = 500;
/// Per-request ceiling for ES calls.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Response body cap — search hits and bulk acks are small.
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// `(id, account_id, content, spoiler_text, created_at, visibility,
/// tombstone, reblog_of_id)` for the status-doc query.
type StatusDocRow = (
    i64,
    i64,
    String,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    String,
    bool,
    Option<i64>,
);

/// The configured backend. Cheap to construct — the `ureq` agent is
/// the only state; index creation is lazy (`ensure` on first use).
pub struct ElasticsearchIndex {
    agent: ureq::Agent,
    /// `http://host:9200` with trailing slashes trimmed.
    base: String,
    prefix: String,
    ensured: AtomicBool,
}

impl ElasticsearchIndex {
    /// Build a client for `url` (`search.elasticsearch_url`) with
    /// index names `{prefix}-statuses` / `{prefix}-accounts`.
    pub fn new(url: &str, prefix: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        ElasticsearchIndex {
            agent,
            base: url.trim_end_matches('/').to_string(),
            prefix: prefix.trim().to_string(),
            ensured: AtomicBool::new(false),
        }
    }

    fn statuses_index(&self) -> String {
        format!("{}-statuses", self.prefix)
    }

    fn accounts_index(&self) -> String {
        format!("{}-accounts", self.prefix)
    }

    /// Create both indices with their mappings when missing. Runs once
    /// per process; a concurrent ensure is harmless (PUT is
    /// idempotent).
    async fn ensure(&self) -> Result<()> {
        if self.ensured.load(Ordering::Relaxed) {
            return Ok(());
        }
        let agent = self.agent.clone();
        let base = self.base.clone();
        let statuses = self.statuses_index();
        let accounts = self.accounts_index();
        tokio::task::spawn_blocking(move || -> Result<()> {
            create_index(&agent, &base, &statuses, statuses_mapping())?;
            create_index(&agent, &base, &accounts, accounts_mapping())?;
            Ok(())
        })
        .await
        .map_err(|e| crate::Error::Internal(format!("es ensure join: {e}")))??;
        self.ensured.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// `POST /{index}/_search` — returns hit `_id`s as i64s.
    async fn candidate_ids(&self, index: &str, query: Value) -> Result<Vec<i64>> {
        self.ensure().await?;
        let agent = self.agent.clone();
        let url = format!("{}/{}/_search", self.base, index);
        let body = json!({
            "size": MAX_CANDIDATES,
            "track_total_hits": true,
            "query": query,
        });
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| crate::Error::Internal(format!("es query serialize: {e}")))?;
        let resp = tokio::task::spawn_blocking(move || {
            agent
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "application/json")
                .send(&bytes[..])
        })
        .await
        .map_err(|e| crate::Error::Internal(format!("es search join: {e}")))?
        .map_err(|e| crate::Error::Internal(format!("es search: {e}")))?;
        let status = resp.status().as_u16();
        let text = read_body(resp)?;
        if !(200..300).contains(&status) {
            return Err(crate::Error::Internal(format!(
                "es search {index} -> {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| crate::Error::Internal(format!("es search parse: {e}")))?;
        let total = doc
            .pointer("/hits/total/value")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if total > MAX_CANDIDATES {
            tracing::warn!(
                index,
                total,
                cap = MAX_CANDIDATES,
                "es match set truncated — results beyond the cap are approximate"
            );
        }
        Ok(doc
            .pointer("/hits/hits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|h| h.get("_id").and_then(Value::as_str))
            .filter_map(|id| id.parse::<i64>().ok())
            .collect())
    }

    /// Index-or-delete one document (`PUT`/`DELETE /{index}/_doc/{id}`).
    async fn doc_op(&self, index: &str, id: i64, body: Option<Value>, delete: bool) -> Result<()> {
        self.ensure().await?;
        let agent = self.agent.clone();
        let url = format!("{}/{}/_doc/{}", self.base, index, id);
        let bytes = match &body {
            Some(b) => serde_json::to_vec(b).unwrap_or_default(),
            None => Vec::new(),
        };
        let resp = tokio::task::spawn_blocking(move || {
            if delete {
                agent.delete(&url).call()
            } else {
                agent
                    .put(&url)
                    .header("content-type", "application/json")
                    .send(&bytes[..])
            }
        })
        .await
        .map_err(|e| crate::Error::Internal(format!("es doc join: {e}")))?
        .map_err(|e| crate::Error::Internal(format!("es doc {index}/{id}: {e}")))?;
        let status = resp.status().as_u16();
        // 404 on delete = already gone — idempotent success.
        if !(200..300).contains(&status) && !(delete && status == 404) {
            let text = read_body(resp).unwrap_or_default();
            return Err(crate::Error::Internal(format!(
                "es doc {index}/{id} -> {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        Ok(())
    }

    /// `POST /{index}/_bulk` — NDJSON action+source pairs.
    async fn bulk(&self, index: &str, actions: &[(String, Option<Value>)]) -> Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        self.ensure().await?;
        let agent = self.agent.clone();
        let url = format!("{}/{}/_bulk", self.base, index);
        let mut ndjson = String::new();
        for (action, source) in actions {
            ndjson.push_str(action);
            ndjson.push('\n');
            if let Some(src) = source {
                ndjson.push_str(&serde_json::to_string(src).unwrap_or_default());
                ndjson.push('\n');
            }
        }
        let bytes = ndjson.into_bytes();
        let resp = tokio::task::spawn_blocking(move || {
            agent
                .post(&url)
                .header("content-type", "application/x-ndjson")
                .header("accept", "application/json")
                .send(&bytes[..])
        })
        .await
        .map_err(|e| crate::Error::Internal(format!("es bulk join: {e}")))?
        .map_err(|e| crate::Error::Internal(format!("es bulk: {e}")))?;
        let status = resp.status().as_u16();
        let text = read_body(resp)?;
        if !(200..300).contains(&status) {
            return Err(crate::Error::Internal(format!(
                "es bulk {index} -> {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| crate::Error::Internal(format!("es bulk parse: {e}")))?;
        if doc.get("errors").and_then(Value::as_bool).unwrap_or(false) {
            return Err(crate::Error::Internal(format!(
                "es bulk {index} reported item errors: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        Ok(())
    }

    /// Load status docs for `ids` — one query, `_bulk` index.
    async fn status_docs(&self, db: &PgPool, ids: &[i64]) -> Result<Vec<(String, Value)>> {
        let rows: Vec<StatusDocRow> = sqlx::query_as(
            "SELECT id, account_id, content, spoiler_text, created_at, \
                        visibility, tombstone, reblog_of_id \
                 FROM statuses WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(db)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    account_id,
                    content,
                    spoiler_text,
                    created_at,
                    visibility,
                    tombstone,
                    reblog,
                )| {
                    let mut doc = json!({
                        "id": id,
                        "account_id": account_id,
                        "content": content,
                        "created_at": created_at.to_rfc3339(),
                        "visibility": visibility,
                        "tombstone": tombstone,
                    });
                    if let Some(spoiler) = spoiler_text {
                        doc["spoiler_text"] = json!(spoiler);
                    }
                    if let Some(reblog) = reblog {
                        doc["reblog_of_id"] = json!(reblog);
                    }
                    (id.to_string(), doc)
                },
            )
            .collect())
    }

    /// Load account docs for `ids` — one query, `_bulk` index.
    async fn account_docs(&self, db: &PgPool, ids: &[i64]) -> Result<Vec<(String, Value)>> {
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, username, display_name, domain FROM accounts WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, username, display_name, domain)| {
                let mut doc = json!({
                    "id": id,
                    "username": username,
                });
                if let Some(name) = display_name {
                    doc["display_name"] = json!(name);
                }
                if let Some(domain) = domain {
                    doc["domain"] = json!(domain);
                }
                (id.to_string(), doc)
            })
            .collect())
    }

    async fn bulk_index(&self, index: &str, docs: Vec<(String, Value)>) -> Result<()> {
        let mut actions = Vec::with_capacity(docs.len());
        for (id, doc) in docs {
            actions.push((
                json!({"index": {"_index": index, "_id": id}}).to_string(),
                Some(doc),
            ));
        }
        for chunk in actions.chunks(BULK_CHUNK) {
            self.bulk(index, chunk).await?;
        }
        Ok(())
    }
}

/// Status text query — token match over content + spoiler with typo
/// tolerance; reblogs and tombstones are filtered here so they never
/// consume candidate slots (Postgres re-checks both anyway).
fn status_query(text: &str, account_id: Option<i64>) -> Value {
    let mut filter = vec![json!({"term": {"tombstone": false}})];
    if let Some(a) = account_id {
        filter.push(json!({"term": {"account_id": a}}));
    }
    json!({
        "bool": {
            "filter": filter,
            "must_not": [{"exists": {"field": "reblog_of_id"}}],
            "must": [{
                "multi_match": {
                    "query": text,
                    "fields": ["content", "spoiler_text"],
                    "type": "best_fields",
                    "fuzziness": "AUTO"
                }
            }]
        }
    })
}

/// Account text query — username (boosted), display name, domain.
/// `prefix` is autocomplete mode (`match_phrase_prefix`).
fn account_query(text: &str, prefix: bool) -> Value {
    if prefix {
        json!({
            "bool": {
                "should": [
                    {"match_phrase_prefix": {"username": text}},
                    {"match_phrase_prefix": {"display_name": text}}
                ],
                "minimum_should_match": 1
            }
        })
    } else {
        json!({
            "bool": {
                "should": [
                    {"match": {"username": {"query": text, "boost": 3.0}}},
                    {"match": {"display_name": {"query": text}}},
                    {"match": {"domain": {"query": text}}}
                ],
                "minimum_should_match": 1
            }
        })
    }
}

#[async_trait::async_trait]
impl SearchIndex for ElasticsearchIndex {
    async fn search_accounts(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
    ) -> Result<Vec<i64>> {
        // FQN is an exact match — cheaper and more precise in Postgres.
        if !matches!(kind, QueryKind::Text(_)) {
            return accounts::search(db, viewer, params, kind).await;
        }
        let QueryKind::Text(text) = kind else {
            unreachable!()
        };
        let prefix = is_prefix_mode(params, kind);
        let t = text.strip_prefix('@').unwrap_or(text);
        match self
            .candidate_ids(&self.accounts_index(), account_query(t, prefix))
            .await
        {
            Ok(ids) => accounts::search_ids(db, viewer, params, kind, &ids).await,
            Err(e) => {
                tracing::warn!(error = %e, "es account search failed — falling back to postgres");
                accounts::search(db, viewer, params, kind).await
            }
        }
    }

    async fn search_statuses(
        &self,
        db: &PgPool,
        viewer: Option<i64>,
        params: &SearchParams,
        kind: &QueryKind,
        hide: Option<&crate::timelines::filter::HideClause>,
    ) -> Result<Vec<StatusRow>> {
        let QueryKind::Text(text) = kind else {
            return Ok(Vec::new());
        };
        match self
            .candidate_ids(
                &self.statuses_index(),
                status_query(text, params.account_id),
            )
            .await
        {
            Ok(ids) => statuses::search_ids(db, viewer, params, kind, hide, &ids).await,
            Err(e) => {
                tracing::warn!(error = %e, "es status search failed — falling back to postgres");
                statuses::search(db, viewer, params, kind, hide).await
            }
        }
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
        // Tag counts need the statuses join — always Postgres.
        hashtags::search(db, viewer, tag, prefix, limit, offset, hide).await
    }

    async fn index_status(&self, db: &PgPool, status_id: i64) -> Result<()> {
        let docs = self.status_docs(db, &[status_id]).await?;
        self.bulk_index(&self.statuses_index(), docs).await
    }

    async fn index_account(&self, db: &PgPool, account_id: i64) -> Result<()> {
        let docs = self.account_docs(db, &[account_id]).await?;
        self.bulk_index(&self.accounts_index(), docs).await
    }

    async fn index_statuses(&self, db: &PgPool, ids: &[i64]) -> Result<()> {
        for chunk in ids.chunks(BULK_CHUNK) {
            let docs = self.status_docs(db, chunk).await?;
            self.bulk_index(&self.statuses_index(), docs).await?;
        }
        Ok(())
    }

    async fn index_accounts(&self, db: &PgPool, ids: &[i64]) -> Result<()> {
        for chunk in ids.chunks(BULK_CHUNK) {
            let docs = self.account_docs(db, chunk).await?;
            self.bulk_index(&self.accounts_index(), docs).await?;
        }
        Ok(())
    }

    async fn remove_status(&self, status_id: i64) -> Result<()> {
        self.doc_op(&self.statuses_index(), status_id, None, true)
            .await
    }

    async fn remove_account(&self, account_id: i64) -> Result<()> {
        self.doc_op(&self.accounts_index(), account_id, None, true)
            .await?;
        // Cascade: the account's statuses leave the index too.
        self.ensure().await?;
        let agent = self.agent.clone();
        let url = format!("{}/{}/_delete_by_query", self.base, self.statuses_index());
        let body = json!({"query": {"term": {"account_id": account_id}}});
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        let resp = tokio::task::spawn_blocking(move || {
            agent
                .post(&url)
                .header("content-type", "application/json")
                .send(&bytes[..])
        })
        .await
        .map_err(|e| crate::Error::Internal(format!("es delete_by_query join: {e}")))?
        .map_err(|e| crate::Error::Internal(format!("es delete_by_query: {e}")))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = read_body(resp).unwrap_or_default();
            return Err(crate::Error::Internal(format!(
                "es delete_by_query -> {status}: {}",
                text.chars().take(500).collect::<String>()
            )));
        }
        Ok(())
    }
}

/// PUT the index with `mapping` when it doesn't exist (HEAD → 404).
fn create_index(agent: &ureq::Agent, base: &str, index: &str, mapping: Value) -> Result<()> {
    let url = format!("{base}/{index}");
    let head = agent.head(&url).call();
    match head {
        Ok(resp) if (200..300).contains(&resp.status().as_u16()) => return Ok(()),
        Ok(resp) if resp.status().as_u16() == 404 => {}
        Ok(resp) => {
            return Err(crate::Error::Internal(format!(
                "es head {index} -> {}",
                resp.status().as_u16()
            )))
        }
        Err(e) => {
            return Err(crate::Error::Internal(format!("es head {index}: {e}")));
        }
    }
    let body = json!({"mappings": mapping});
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let resp = agent
        .put(&url)
        .header("content-type", "application/json")
        .send(&bytes[..])
        .map_err(|e| crate::Error::Internal(format!("es create {index}: {e}")))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = read_body(resp).unwrap_or_default();
        // A concurrent ensure can win the HEAD→PUT race — the index
        // existing is the desired end state either way.
        if text.contains("resource_already_exists_exception") {
            return Ok(());
        }
        return Err(crate::Error::Internal(format!(
            "es create {index} -> {status}: {}",
            text.chars().take(500).collect::<String>()
        )));
    }
    tracing::info!(index, "created elasticsearch index");
    Ok(())
}

fn read_body(resp: ureq::http::Response<ureq::Body>) -> Result<String> {
    let mut resp = resp;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| crate::Error::Internal(format!("es body read: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn statuses_mapping() -> Value {
    json!({
        "properties": {
            "id": {"type": "long"},
            "account_id": {"type": "long"},
            "content": {"type": "text"},
            "spoiler_text": {"type": "text"},
            "created_at": {"type": "date"},
            "visibility": {"type": "keyword"},
            "tombstone": {"type": "boolean"},
            "reblog_of_id": {"type": "long"}
        }
    })
}

fn accounts_mapping() -> Value {
    json!({
        "properties": {
            "id": {"type": "long"},
            "username": {
                "type": "text",
                "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}
            },
            "display_name": {"type": "text"},
            "domain": {"type": "keyword"}
        }
    })
}
