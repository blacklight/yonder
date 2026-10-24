//! Followed hashtags — `tag_follows` CRUD and the followed-tag list.

use sqlx::{PgPool, Row};

use crate::{http::paginate::Page, Result};

use super::{lookup, normalize_name, TagInfo};

/// `POST /api/v1/tags/{name}/follow` — idempotent per UNIQUE
/// `(account_id, tag_id)`. The `tags` row is upserted so following a
/// never-seen tag still works.
pub async fn follow(
    db: &PgPool,
    account_id: i64,
    name: &str,
    include_remote: bool,
) -> Result<TagInfo> {
    let name = normalize_name(name)?;
    let tag_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) \
         DO UPDATE SET name = EXCLUDED.name RETURNING id",
    )
    .bind(&name)
    .fetch_one(db)
    .await?;
    sqlx::query(
        "INSERT INTO tag_follows (account_id, tag_id) VALUES ($1,$2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(account_id)
    .bind(tag_id)
    .execute(db)
    .await?;
    lookup(db, Some(account_id), &name, include_remote)
        .await?
        .ok_or_else(|| crate::Error::NotFound("tag".into()))
}

/// `POST /api/v1/tags/{name}/unfollow` — idempotent. `None` when the
/// tag does not exist (the endpoint maps it to 404).
pub async fn unfollow(
    db: &PgPool,
    account_id: i64,
    name: &str,
    include_remote: bool,
) -> Result<Option<TagInfo>> {
    let name = normalize_name(name)?;
    let tag_id = sqlx::query_scalar::<_, i64>("SELECT id FROM tags WHERE lower(name) = $1")
        .bind(&name)
        .fetch_optional(db)
        .await?;
    let Some(tag_id) = tag_id else {
        return Ok(None);
    };
    sqlx::query("DELETE FROM tag_follows WHERE account_id = $1 AND tag_id = $2")
        .bind(account_id)
        .bind(tag_id)
        .execute(db)
        .await?;
    Ok(Some(
        lookup(db, Some(account_id), &name, include_remote)
            .await?
            .ok_or_else(|| crate::Error::NotFound("tag".into()))?,
    ))
}

/// `GET /api/v1/followed_tags` — the viewer's followed tags,
/// newest-first, cursored on `tag_follows.id`. Each item pairs the
/// [`TagInfo`] with its pagination cursor.
pub async fn followed(
    db: &PgPool,
    account_id: i64,
    page: &Page,
    include_remote: bool,
) -> Result<Vec<(TagInfo, i64)>> {
    let mut sql = String::from(
        "SELECT t.id, t.name, tf.id AS follow_id FROM tag_follows tf \
         JOIN tags t ON t.id = tf.tag_id WHERE tf.account_id = $1",
    );
    let mut bind_idx = 2;
    if page.max_id.is_some() {
        sql.push_str(&format!(" AND tf.id < ${bind_idx}"));
        bind_idx += 1;
    }
    if page.since_id.is_some() {
        sql.push_str(&format!(" AND tf.id > ${bind_idx}"));
        bind_idx += 1;
    }
    sql.push_str(&format!(" ORDER BY tf.id DESC LIMIT ${bind_idx}"));
    let mut q = sqlx::query(&sql).bind(account_id);
    if let Some(max_id) = page.max_id {
        q = q.bind(max_id);
    }
    if let Some(since_id) = page.since_id {
        q = q.bind(since_id);
    }
    let rows = q.bind(page.limit).fetch_all(db).await?;
    // One grouped history query for the whole page (W11).
    let ids: Vec<i64> = rows.iter().map(|r| r.get("id")).collect();
    let histories = super::trends::history_for_many(db, &ids, 7, include_remote).await?;
    Ok(rows
        .iter()
        .zip(histories)
        .map(|(row, history)| {
            (
                TagInfo {
                    id: row.get("id"),
                    name: row.get("name"),
                    following: true,
                    history,
                },
                row.get("follow_id"),
            )
        })
        .collect())
}
