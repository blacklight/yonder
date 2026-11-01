//! Postgres-backed `webmentions_sdk::WebmentionsStorage` — the
//! delivery ledger is the `webmentions` table (migration 0040).
//!
//! The SDK's trait is synchronous and the outgoing processor calls
//! it from its own `std::thread::scope` delivery workers — not tokio
//! threads — so the bridge can't rely on `Handle::current()`. The
//! runtime handle is captured at construction instead:
//! `handle.block_on` drives the sqlx future on the right reactor
//! from any thread, and `block_in_place` frees the worker slot when
//! the caller *is* on the runtime.

use sqlx::PgPool;
use tokio::runtime::Handle;
use webmentions_sdk::{
    storage::WebmentionsStorage, Webmention, WebmentionDirection, WebmentionStatus,
};

/// Shared storage handle — one per `JobContext` use site.
pub struct PgWebmentionsStorage {
    pool: PgPool,
    /// The runtime async work executes on — captured from the job's
    /// tokio context so delivery threads can drive sqlx futures.
    handle: Handle,
}

impl PgWebmentionsStorage {
    /// Create a storage over the given pool. Must be called from
    /// within the tokio runtime (the job handler's async context).
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            handle: Handle::current(),
        }
    }

    /// Bridge a sqlx future to sync. On the runtime: `block_in_place`
    /// frees the worker slot (the pubby pattern). Off it — the SDK's
    /// delivery threads — the captured handle runs the future under
    /// the runtime's reactor.
    fn sync_run<F: std::future::Future>(&self, f: F) -> F::Output {
        match Handle::try_current() {
            Ok(_) => tokio::task::block_in_place(|| self.handle.block_on(f)),
            Err(_) => self.handle.block_on(f),
        }
    }
}

fn direction_str(direction: WebmentionDirection) -> &'static str {
    direction.as_str()
}

/// Map a `webmentions` table row back to the SDK model.
fn row_to_mention(row: &sqlx::postgres::PgRow) -> webmentions_sdk::Result<Webmention> {
    use sqlx::Row;
    let direction = WebmentionDirection::from_raw(row.get::<&str, _>("direction"))
        .map_err(|e| webmentions_sdk::WebmentionError::InvalidValue(e.to_string()))?;
    let status = WebmentionStatus::from_raw(row.get::<&str, _>("status"))
        .map_err(|e| webmentions_sdk::WebmentionError::InvalidValue(e.to_string()))?;
    let mention_type =
        webmentions_sdk::WebmentionType::from_raw(Some(row.get::<&str, _>("mention_type")));
    Ok(Webmention {
        source: row.get("source"),
        target: row.get("target"),
        direction,
        title: row.get("title"),
        excerpt: row.get("excerpt"),
        content: row.get("content"),
        author_name: row.get("author_name"),
        author_url: row.get("author_url"),
        author_photo: row.get("author_photo"),
        published: row.get("published"),
        status,
        mention_type,
        mention_type_raw: row.get("mention_type_raw"),
        metadata: row.get("metadata"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

/// sqlx → SDK error.
fn db_err(e: sqlx::Error) -> webmentions_sdk::WebmentionError {
    webmentions_sdk::WebmentionError::General(e.to_string())
}

impl WebmentionsStorage for PgWebmentionsStorage {
    /// Upsert on `(source, target, direction)` — re-verified mentions
    /// update in place, keeping `created_at` and bumping `updated_at`.
    fn store_webmention(&self, mention: &Webmention) -> webmentions_sdk::Result<()> {
        let pool = self.pool.clone();
        let mention = mention.clone();
        self.sync_run(async move {
            sqlx::query(
                "INSERT INTO webmentions (source, target, direction, status, mention_type, \
                     mention_type_raw, title, excerpt, content, author_name, author_url, \
                     author_photo, published, metadata, created_at, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14, \
                     COALESCE($15, now()), COALESCE($16, now())) \
                 ON CONFLICT (source, target, direction) DO UPDATE SET \
                     status = EXCLUDED.status, mention_type = EXCLUDED.mention_type, \
                     mention_type_raw = EXCLUDED.mention_type_raw, \
                     title = EXCLUDED.title, excerpt = EXCLUDED.excerpt, \
                     content = EXCLUDED.content, \
                     author_name = EXCLUDED.author_name, \
                     author_url = EXCLUDED.author_url, \
                     author_photo = EXCLUDED.author_photo, \
                     published = EXCLUDED.published, \
                     metadata = EXCLUDED.metadata, \
                     updated_at = now()",
            )
            .bind(&mention.source)
            .bind(&mention.target)
            .bind(direction_str(mention.direction))
            .bind(mention.status.as_str())
            .bind(mention.mention_type.as_str())
            .bind(&mention.mention_type_raw)
            .bind(&mention.title)
            .bind(&mention.excerpt)
            .bind(&mention.content)
            .bind(&mention.author_name)
            .bind(&mention.author_url)
            .bind(&mention.author_photo)
            .bind(mention.published)
            .bind(&mention.metadata)
            .bind(mention.created_at)
            .bind(mention.updated_at)
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(db_err)
        })
    }

    /// Soft-delete — the row stays as the ledger record.
    fn delete_webmention(
        &self,
        source: &str,
        target: &str,
        direction: WebmentionDirection,
    ) -> webmentions_sdk::Result<()> {
        let pool = self.pool.clone();
        let (source, target) = (source.to_string(), target.to_string());
        self.sync_run(async move {
            sqlx::query(
                "UPDATE webmentions SET status = 'deleted', updated_at = now() \
                 WHERE source = $1 AND target = $2 AND direction = $3",
            )
            .bind(&source)
            .bind(&target)
            .bind(direction_str(direction))
            .execute(&pool)
            .await
            .map(|_| ())
            .map_err(db_err)
        })
    }

    /// `resource` is the target for incoming rows, the source for
    /// outgoing ones (matching the SDK contract).
    fn retrieve_webmentions(
        &self,
        resource: &str,
        direction: WebmentionDirection,
    ) -> webmentions_sdk::Result<Vec<Webmention>> {
        let pool = self.pool.clone();
        let resource = resource.to_string();
        let column = match direction {
            WebmentionDirection::In => "target",
            WebmentionDirection::Out => "source",
        };
        let sql = format!(
            "SELECT source, target, direction, status, mention_type, mention_type_raw, \
                 title, excerpt, content, author_name, author_url, author_photo, \
                 published, metadata, created_at, updated_at \
             FROM webmentions WHERE {column} = $1 AND direction = $2 AND status <> 'deleted'"
        );
        self.sync_run(async move {
            let rows = sqlx::query(&sql)
                .bind(&resource)
                .bind(direction_str(direction))
                .fetch_all(&pool)
                .await
                .map_err(db_err)?;
            rows.iter().map(row_to_mention).collect()
        })
    }
}
