//! `filters` / `filter_keywords` / `filter_statuses` CRUD — all
//! owner-scoped (`WHERE account_id = $owner`); a foreign or missing
//! id surfaces as `Error::NotFound`.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::{Error, Result};

/// One `filters` row.
#[derive(Debug)]
pub struct FilterRow {
    pub id: i64,
    pub title: String,
    pub context: Vec<String>,
    pub action: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// One `filter_keywords` row.
#[derive(Debug)]
pub struct KeywordRow {
    pub id: i64,
    pub filter_id: i64,
    pub keyword: String,
    pub whole_word: bool,
}

/// Contexts a filter may apply to (the `filters.context` CHECK list).
pub const CONTEXTS: &[&str] = &["home", "notifications", "public", "thread", "account"];

/// Validate a `context[]` input: non-empty, subset of [`CONTEXTS`],
/// deduplicated.
pub fn validate_context(context: &[String]) -> Result<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(context.len());
    for c in context {
        if !CONTEXTS.contains(&c.as_str()) {
            return Err(Error::Validation(format!("unknown filter context '{c}'")));
        }
        if seen.insert(c.clone()) {
            out.push(c.clone());
        }
    }
    if out.is_empty() {
        return Err(Error::Validation(
            "filters require at least one context".to_string(),
        ));
    }
    Ok(out)
}

/// `warn|hide|blur` — the only actions the schema accepts.
pub fn validate_action(action: &str) -> Result<()> {
    match action {
        "warn" | "hide" | "blur" => Ok(()),
        _ => Err(Error::Validation(format!(
            "unknown filter_action '{action}'"
        ))),
    }
}

fn validate_title(title: &str) -> Result<String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(Error::Validation("filter title is required".to_string()));
    }
    if title.chars().count() > 500 {
        return Err(Error::Validation("filter title is too long".to_string()));
    }
    Ok(title)
}

fn validate_keyword(keyword: &str) -> Result<String> {
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() {
        return Err(Error::Validation("keyword is required".to_string()));
    }
    if keyword.chars().count() > 500 {
        return Err(Error::Validation("keyword is too long".to_string()));
    }
    Ok(keyword)
}

fn row_to_filter(r: &sqlx::postgres::PgRow) -> FilterRow {
    FilterRow {
        id: r.get("id"),
        title: r.get("title"),
        context: r.get("context"),
        action: r.get("action"),
        expires_at: r.get("expires_at"),
    }
}

const FILTER_COLS: &str = "id, title, context, action, expires_at";

/// Load a filter owned by `owner`; expired rows are still visible to
/// CRUD (Mastodon shows them — they are simply inert at read time).
pub async fn get(db: &PgPool, owner: i64, id: i64) -> Result<FilterRow> {
    let row = sqlx::query(&format!(
        "SELECT {FILTER_COLS} FROM filters WHERE id = $1 AND account_id = $2"
    ))
    .bind(id)
    .bind(owner)
    .fetch_optional(db)
    .await?;
    row.as_ref()
        .map(row_to_filter)
        .ok_or_else(|| Error::NotFound("filter".into()))
}

/// All of `owner`'s filters, newest-first.
pub async fn list(db: &PgPool, owner: i64) -> Result<Vec<FilterRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {FILTER_COLS} FROM filters WHERE account_id = $1 ORDER BY id DESC"
    ))
    .bind(owner)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_filter).collect())
}

/// `expires_in` seconds → an absolute timestamp; `None`/`0` means
/// never expire (Mastodon's update semantic — `0` clears it).
fn expiry_from_seconds(expires_in: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    match expires_in {
        None | Some(0) => Ok(None),
        Some(n) if n > 0 => Ok(Some(Utc::now() + chrono::Duration::seconds(n))),
        Some(_) => Err(Error::Validation(
            "expires_in must be a positive number of seconds".to_string(),
        )),
    }
}

/// Create-time `expires_in` — must be a positive count of seconds;
/// `0` (clear) only has meaning on update.
fn create_expiry_from_seconds(expires_in: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    match expires_in {
        Some(0) => Err(Error::Validation(
            "expires_in must be a positive number of seconds".to_string(),
        )),
        other => expiry_from_seconds(other),
    }
}

/// Insert a filter plus optional keywords in one transaction.
pub async fn create(
    db: &PgPool,
    owner: i64,
    title: &str,
    context: &[String],
    action: &str,
    expires_in: Option<i64>,
    keywords: &[(String, bool)],
) -> Result<FilterRow> {
    let title = validate_title(title)?;
    let context = validate_context(context)?;
    validate_action(action)?;
    let expires_at = create_expiry_from_seconds(expires_in)?;
    let mut tx = db.begin().await?;
    let row = sqlx::query(&format!(
        "INSERT INTO filters (account_id, title, context, action, expires_at) \
         VALUES ($1,$2,$3,$4,$5) RETURNING {FILTER_COLS}"
    ))
    .bind(owner)
    .bind(&title)
    .bind(&context)
    .bind(action)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;
    let filter = row_to_filter(&row);
    insert_keywords(&mut tx, filter.id, keywords).await?;
    tx.commit().await?;
    Ok(filter)
}

async fn insert_keywords(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    filter_id: i64,
    keywords: &[(String, bool)],
) -> Result<()> {
    for (keyword, whole_word) in keywords {
        let keyword = validate_keyword(keyword)?;
        sqlx::query(
            "INSERT INTO filter_keywords (filter_id, keyword, whole_word) \
             VALUES ($1,$2,$3)",
        )
        .bind(filter_id)
        .bind(&keyword)
        .bind(whole_word)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Partial update — any of `title`/`context`/`action`/`expires_in`
/// may change; `expires_in = 0` clears the expiry.
pub async fn update(
    db: &PgPool,
    owner: i64,
    id: i64,
    title: Option<&str>,
    context: Option<&[String]>,
    action: Option<&str>,
    expires_in: Option<i64>,
) -> Result<FilterRow> {
    let existing = get(db, owner, id).await?;
    let title = match title {
        Some(t) => validate_title(t)?,
        None => existing.title,
    };
    let context = match context {
        Some(c) => validate_context(c)?,
        None => existing.context,
    };
    let action = match action {
        Some(a) => {
            validate_action(a)?;
            a.to_string()
        }
        None => existing.action,
    };
    let expires_at = match expires_in {
        Some(n) => expiry_from_seconds(Some(n))?,
        None => existing.expires_at,
    };
    let row = sqlx::query(&format!(
        "UPDATE filters SET title = $3, context = $4, action = $5, expires_at = $6, \
                updated_at = now() WHERE id = $1 AND account_id = $2 RETURNING {FILTER_COLS}"
    ))
    .bind(id)
    .bind(owner)
    .bind(&title)
    .bind(&context)
    .bind(&action)
    .bind(expires_at)
    .fetch_one(db)
    .await?;
    Ok(row_to_filter(&row))
}

/// `DELETE` — cascades to keywords and status pins.
pub async fn delete(db: &PgPool, owner: i64, id: i64) -> Result<()> {
    let affected = sqlx::query("DELETE FROM filters WHERE id = $1 AND account_id = $2")
        .bind(id)
        .bind(owner)
        .execute(db)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(Error::NotFound("filter".into()));
    }
    Ok(())
}

/// Keywords of a filter the caller owns (foreign/missing → 404).
pub async fn keywords(db: &PgPool, owner: i64, filter_id: i64) -> Result<Vec<KeywordRow>> {
    get(db, owner, filter_id).await?;
    let rows = sqlx::query(
        "SELECT id, filter_id, keyword, whole_word FROM filter_keywords \
         WHERE filter_id = $1 ORDER BY id",
    )
    .bind(filter_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| KeywordRow {
            id: r.get("id"),
            filter_id: r.get("filter_id"),
            keyword: r.get("keyword"),
            whole_word: r.get("whole_word"),
        })
        .collect())
}

/// Add a keyword, enforcing the per-filter cap. The parent row is
/// locked `FOR UPDATE` so concurrent adds can't race past the cap.
pub async fn add_keyword(
    db: &PgPool,
    owner: i64,
    filter_id: i64,
    keyword: &str,
    whole_word: bool,
    max_keywords: u32,
) -> Result<KeywordRow> {
    let keyword = validate_keyword(keyword)?;
    let mut tx = db.begin().await?;
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM filters WHERE id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(filter_id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::NotFound("filter".into()))?;
    let row = sqlx::query(
        "INSERT INTO filter_keywords (filter_id, keyword, whole_word) \
         SELECT $1,$2,$3 \
         WHERE (SELECT COUNT(*) FROM filter_keywords WHERE filter_id = $1) < $4 \
         RETURNING id, filter_id, keyword, whole_word",
    )
    .bind(filter_id)
    .bind(&keyword)
    .bind(whole_word)
    .bind(max_keywords as i64)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::Validation(format!("at most {max_keywords} keywords per filter")))?;
    tx.commit().await?;
    Ok(KeywordRow {
        id: row.get("id"),
        filter_id: row.get("filter_id"),
        keyword: row.get("keyword"),
        whole_word: row.get("whole_word"),
    })
}

/// Load one keyword, scoped through the owner's filter.
pub async fn get_keyword(db: &PgPool, owner: i64, keyword_id: i64) -> Result<KeywordRow> {
    let row = sqlx::query(
        "SELECT k.id, k.filter_id, k.keyword, k.whole_word FROM filter_keywords k \
         JOIN filters f ON f.id = k.filter_id \
         WHERE k.id = $1 AND f.account_id = $2",
    )
    .bind(keyword_id)
    .bind(owner)
    .fetch_optional(db)
    .await?;
    row.map(|r| KeywordRow {
        id: r.get("id"),
        filter_id: r.get("filter_id"),
        keyword: r.get("keyword"),
        whole_word: r.get("whole_word"),
    })
    .ok_or_else(|| Error::NotFound("filter_keyword".into()))
}

/// Update a keyword's text/whole_word flag.
pub async fn update_keyword(
    db: &PgPool,
    owner: i64,
    keyword_id: i64,
    keyword: Option<&str>,
    whole_word: Option<bool>,
) -> Result<KeywordRow> {
    let existing = get_keyword(db, owner, keyword_id).await?;
    let kw = match keyword {
        Some(k) => validate_keyword(k)?,
        None => existing.keyword,
    };
    let row = sqlx::query(
        "UPDATE filter_keywords SET keyword = $2, whole_word = $3 WHERE id = $1 \
         RETURNING id, filter_id, keyword, whole_word",
    )
    .bind(keyword_id)
    .bind(&kw)
    .bind(whole_word.unwrap_or(existing.whole_word))
    .fetch_one(db)
    .await?;
    Ok(KeywordRow {
        id: row.get("id"),
        filter_id: row.get("filter_id"),
        keyword: row.get("keyword"),
        whole_word: row.get("whole_word"),
    })
}

/// Remove a keyword from an owned filter.
pub async fn delete_keyword(db: &PgPool, owner: i64, keyword_id: i64) -> Result<()> {
    get_keyword(db, owner, keyword_id).await?;
    sqlx::query("DELETE FROM filter_keywords WHERE id = $1")
        .bind(keyword_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Statuses pinned to a filter — `(filter_statuses.id, status_id)`
/// pairs (`id` is the FilterStatus entity id).
pub async fn pinned_statuses(db: &PgPool, owner: i64, filter_id: i64) -> Result<Vec<(i64, i64)>> {
    get(db, owner, filter_id).await?;
    let rows =
        sqlx::query("SELECT id, status_id FROM filter_statuses WHERE filter_id = $1 ORDER BY id")
            .bind(filter_id)
            .fetch_all(db)
            .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("id"), r.get::<i64, _>("status_id")))
        .collect())
}

/// Pin a status to a filter — returns the `filter_statuses` row id
/// (the FilterStatus entity id); idempotent per UNIQUE
/// `(filter_id, status_id)`.
pub async fn pin_status(db: &PgPool, owner: i64, filter_id: i64, status_id: i64) -> Result<i64> {
    get(db, owner, filter_id).await?;
    Ok(sqlx::query_scalar::<_, i64>(
        "INSERT INTO filter_statuses (filter_id, status_id) VALUES ($1,$2) \
         ON CONFLICT (filter_id, status_id) DO UPDATE SET status_id = EXCLUDED.status_id \
         RETURNING id",
    )
    .bind(filter_id)
    .bind(status_id)
    .fetch_one(db)
    .await?)
}

/// Remove a status pin — `NotFound("filter_status")` for foreign ids.
pub async fn unpin_status(db: &PgPool, owner: i64, pin_id: i64) -> Result<()> {
    let affected = sqlx::query(
        "DELETE FROM filter_statuses fs USING filters f \
         WHERE fs.id = $1 AND fs.filter_id = f.id AND f.account_id = $2",
    )
    .bind(pin_id)
    .bind(owner)
    .execute(db)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(Error::NotFound("filter_status".into()));
    }
    Ok(())
}
