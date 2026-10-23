//! Lists — `lists`/`list_accounts` CRUD and the membership queries
//! epic 15's list timeline consumes. Ownership is the `account_id`
//! column: reads and writes 404 on foreign lists.

use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::{http::paginate::Page, Error, Result};

/// A `lists` row as read off the database.
#[derive(Debug, Clone)]
pub struct ListRow {
    pub id: i64,
    pub account_id: i64,
    pub title: String,
    pub replies_policy: String,
    pub exclusive: bool,
}

impl ListRow {
    /// The Mastodon `List` entity.
    pub fn entity(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "title": self.title,
            "replies_policy": self.replies_policy,
            "exclusive": self.exclusive,
        })
    }
}

const LIST_COLS: &str = "id, account_id, title, replies_policy, exclusive";

fn list_row(r: &sqlx::postgres::PgRow) -> ListRow {
    ListRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        title: r.get("title"),
        replies_policy: r.get("replies_policy"),
        exclusive: r.get("exclusive"),
    }
}

/// The caller's lists, newest first.
pub async fn list(db: &PgPool, owner_id: i64, page: &Page) -> Result<Vec<ListRow>> {
    let query = format!(
        "SELECT {LIST_COLS} FROM lists \
         WHERE account_id = $1 \
           AND ($2::bigint IS NULL OR id < $2) \
           AND ($3::bigint IS NULL OR id > $3) \
         ORDER BY id DESC LIMIT $4"
    );
    let rows = sqlx::query(&query)
        .bind(owner_id)
        .bind(page.max_id)
        .bind(page.since_id)
        .bind(page.limit)
        .fetch_all(db)
        .await?;
    Ok(rows.iter().map(list_row).collect())
}

/// Create a list for `owner_id`. `replies_policy` must be one of the
/// CHECK'd values; `title` is required non-empty (≤ 500 chars).
pub async fn create(
    db: &PgPool,
    owner_id: i64,
    title: &str,
    replies_policy: &str,
    exclusive: bool,
) -> Result<ListRow> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 500 {
        return Err(Error::Validation("list title is required".into()));
    }
    if !matches!(replies_policy, "followed" | "list" | "none") {
        return Err(Error::Validation("invalid replies_policy".into()));
    }
    let row = sqlx::query(&format!(
        "INSERT INTO lists (account_id, title, replies_policy, exclusive) \
             VALUES ($1, $2, $3, $4) RETURNING {LIST_COLS}"
    ))
    .bind(owner_id)
    .bind(title)
    .bind(replies_policy)
    .bind(exclusive)
    .fetch_one(db)
    .await?;
    Ok(list_row(&row))
}

/// Fetch `id` — `None` unless the caller owns it.
pub async fn get(db: &PgPool, owner_id: i64, id: i64) -> Result<Option<ListRow>> {
    let row = sqlx::query(&format!(
        "SELECT {LIST_COLS} FROM lists WHERE id = $1 AND account_id = $2"
    ))
    .bind(id)
    .bind(owner_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| list_row(&r)))
}

/// Fetch `id` or 404 — the uniform entry point for member routes.
pub async fn get_owned(db: &PgPool, owner_id: i64, id: i64) -> Result<ListRow> {
    get(db, owner_id, id)
        .await?
        .ok_or_else(|| Error::NotFound("list".into()))
}

/// Update `id` — any of title/replies_policy/exclusive may change.
/// Returns the updated row or `None` on foreign lists.
pub async fn update(
    db: &PgPool,
    owner_id: i64,
    id: i64,
    title: Option<&str>,
    replies_policy: Option<&str>,
    exclusive: Option<bool>,
) -> Result<Option<ListRow>> {
    let title = title.map(str::trim);
    if let Some(t) = title {
        if t.is_empty() || t.chars().count() > 500 {
            return Err(Error::Validation("list title is required".into()));
        }
    }
    if let Some(p) = replies_policy {
        if !matches!(p, "followed" | "list" | "none") {
            return Err(Error::Validation("invalid replies_policy".into()));
        }
    }
    let row = sqlx::query(&format!(
        "UPDATE lists SET \
             title = COALESCE($3, title), \
             replies_policy = COALESCE($4, replies_policy), \
             exclusive = COALESCE($5, exclusive), \
             updated_at = now() \
         WHERE id = $1 AND account_id = $2 \
         RETURNING {LIST_COLS}"
    ))
    .bind(id)
    .bind(owner_id)
    .bind(title)
    .bind(replies_policy)
    .bind(exclusive)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| list_row(&r)))
}

/// Delete `id` — cascades its `list_accounts` rows.
pub async fn delete(db: &PgPool, owner_id: i64, id: i64) -> Result<bool> {
    let done = sqlx::query("DELETE FROM lists WHERE id = $1 AND account_id = $2 RETURNING id")
        .bind(id)
        .bind(owner_id)
        .fetch_optional(db)
        .await?;
    Ok(done.is_some())
}

/// Members of `list_id` — newest `account_id` first (account ids are
/// monotonic), `(account_id, cursor)`.
pub async fn members(
    db: &PgPool,
    owner_id: i64,
    list_id: i64,
    page: &Page,
) -> Result<Vec<(i64, i64)>> {
    get_owned(db, owner_id, list_id).await?;
    let rows = sqlx::query(
        "SELECT a.id AS account_id, la.account_id AS cursor \
         FROM list_accounts la JOIN accounts a ON a.id = la.account_id \
         WHERE la.list_id = $1 \
           AND a.suspended_at IS NULL AND a.deleted_at IS NULL \
           AND ($2::bigint IS NULL OR la.account_id < $2) \
           AND ($3::bigint IS NULL OR la.account_id > $3) \
         ORDER BY la.account_id DESC LIMIT $4",
    )
    .bind(list_id)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("account_id"), r.get("cursor")))
        .collect())
}

/// Add `account_ids` to `list_id` — each must resolve to a live
/// account (Mastodon no longer requires following the member);
/// idempotent via the `(list_id, account_id)` PK.
pub async fn add_accounts(
    db: &PgPool,
    owner_id: i64,
    list_id: i64,
    account_ids: &[i64],
) -> Result<()> {
    get_owned(db, owner_id, list_id).await?;
    for id in account_ids {
        if !super::visibility::visible(db, *id).await? {
            return Err(Error::Validation(format!("account {id} is not listable")));
        }
    }
    sqlx::query(
        "INSERT INTO list_accounts (list_id, account_id) \
         SELECT $1, unnest($2::bigint[]) \
         ON CONFLICT (list_id, account_id) DO NOTHING",
    )
    .bind(list_id)
    .bind(account_ids)
    .execute(db)
    .await?;
    Ok(())
}

/// Remove `account_ids` from `list_id`.
pub async fn remove_accounts(
    db: &PgPool,
    owner_id: i64,
    list_id: i64,
    account_ids: &[i64],
) -> Result<()> {
    get_owned(db, owner_id, list_id).await?;
    sqlx::query("DELETE FROM list_accounts WHERE list_id = $1 AND account_id = ANY($2::bigint[])")
        .bind(list_id)
        .bind(account_ids)
        .execute(db)
        .await?;
    Ok(())
}

/// The caller's lists containing `account_id` — powers
/// `GET /api/v1/accounts/{id}/lists`.
pub async fn containing(db: &PgPool, owner_id: i64, account_id: i64) -> Result<Vec<ListRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {LIST_COLS} FROM lists l \
         WHERE l.account_id = $1 \
           AND EXISTS(SELECT 1 FROM list_accounts la \
                      WHERE la.list_id = l.id AND la.account_id = $2) \
         ORDER BY l.id DESC"
    ))
    .bind(owner_id)
    .bind(account_id)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(list_row).collect())
}
