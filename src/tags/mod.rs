//! Hashtag management — lookup, follow/unfollow, followed-tag list
//! and trending (epic 20). The tag *timeline* lives in epic 15
//! (`TimelineScope::Tag`); this module owns the tag CRUD surface and
//! the shared Tag entity.

pub mod entity;
pub mod follow;
pub mod trends;

use sqlx::{PgPool, Row};

use crate::{Error, Result};

/// A Tag entity's inputs: the row, the viewer's follow state and the
/// trailing usage history.
#[derive(Debug)]
pub struct TagInfo {
    pub id: i64,
    pub name: String,
    pub following: bool,
    pub history: Vec<entity::HistoryDay>,
}

/// `tags.name` — letters, digits and underscores, ≤ 200 chars (the
/// column CHECK). Lowercased before use; matching is `lower(name)`.
pub fn normalize_name(name: &str) -> Result<String> {
    let name = name.trim().to_lowercase();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(Error::Validation("invalid hashtag name".to_string()));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(Error::Validation("invalid hashtag name".to_string()));
    }
    Ok(name)
}

/// Load a tag by name (case-insensitive) with the viewer's follow
/// flag and 7-day history. `None` when the tag has no rows.
pub async fn lookup(
    db: &PgPool,
    viewer: Option<i64>,
    name: &str,
    include_remote: bool,
) -> Result<Option<TagInfo>> {
    let name = normalize_name(name)?;
    let row = sqlx::query("SELECT id, name FROM tags WHERE lower(name) = $1")
        .bind(&name)
        .fetch_optional(db)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let id: i64 = row.get("id");
    let following = match viewer {
        Some(viewer) => {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM tag_follows \
                 WHERE account_id = $1 AND tag_id = $2)",
            )
            .bind(viewer)
            .bind(id)
            .fetch_one(db)
            .await?
        }
        None => false,
    };
    let history = trends::history_for(db, id, 7, include_remote).await?;
    Ok(Some(TagInfo {
        id,
        name: row.get("name"),
        following,
        history,
    }))
}
