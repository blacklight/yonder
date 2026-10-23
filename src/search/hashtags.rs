//! Hashtag search — `lower(tags.name)` matching, visibility-aware.
//! Prefix mode powers autocomplete (`type=hashtags`); plain text
//! searches match exactly.

use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::timelines::filter::Bind;
use crate::timelines::query::bind_all;
use crate::Result;

use super::like_pattern;
use super::statuses::base_clauses;

/// One matching tag with its visible-status count.
#[derive(Debug)]
pub struct TagHit {
    pub name: String,
    /// Lifetime count of statuses carrying the tag that the caller
    /// may see — a yonder extension (`yonder.statuses_count`).
    pub statuses_count: i64,
    /// Whether the viewer follows the tag (`tag_follows`).
    pub following: bool,
}

/// Match `tag` against `tags.name`. `prefix=true` matches
/// `lower(name) LIKE 'tag%'` (autocomplete); `false` is exact.
/// Tags only surface when the caller can see at least one carrying
/// status — the join double-serves as count and visibility gate.
pub async fn search(
    db: &PgPool,
    viewer: Option<i64>,
    tag: &str,
    prefix: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<TagHit>> {
    let mut binds: Vec<Bind> = Vec::new();
    let mut clauses = base_clauses(viewer, &mut binds);
    let mut alloc = |b: Bind| {
        binds.push(b);
        format!("${}", binds.len())
    };
    let match_clause = if prefix {
        let p = alloc(Bind::Text(like_pattern(&tag.to_lowercase(), true)));
        format!("lower(t.name) LIKE {p}")
    } else {
        let p = alloc(Bind::Text(tag.to_lowercase()));
        format!("lower(t.name) = {p}")
    };
    clauses.push(match_clause);

    let following = match viewer {
        // $1 is the viewer (bound by base_clauses).
        Some(_) => "EXISTS (SELECT 1 FROM tag_follows tf \
                    WHERE tf.account_id = $1 AND tf.tag_id = t.id) AS following"
            .to_string(),
        None => "false AS following".to_string(),
    };
    let limit_p = alloc(Bind::I64(limit));
    let offset_p = alloc(Bind::I64(offset));
    let sql = format!(
        "SELECT t.name, COUNT(DISTINCT s.id) AS statuses_count, {following} \
         FROM tags t \
         JOIN status_tags st ON st.tag_id = t.id \
         JOIN statuses s ON s.id = st.status_id \
         JOIN accounts a ON a.id = s.account_id \
         WHERE {} \
         GROUP BY t.id, t.name \
         ORDER BY statuses_count DESC, t.name \
         LIMIT {limit_p} OFFSET {offset_p}",
        clauses.join(" AND ")
    );
    let q = bind_all(sqlx::query(&sql), binds.iter());
    let rows = q.fetch_all(db).await?;
    Ok(rows
        .iter()
        .map(|r| TagHit {
            name: r.get("name"),
            statuses_count: r.get("statuses_count"),
            following: r.get("following"),
        })
        .collect())
}

/// The Mastodon Tag entity (v2). `history` stays empty — trending is
/// epic 20. `yonder.statuses_count` carries the lifetime count.
pub fn tag_entity(base_url: &str, hit: &TagHit) -> Value {
    json!({
        "name": hit.name,
        "url": format!("{}/tags/{}", base_url, hit.name),
        "history": [],
        "following": hit.following,
        "yonder": {
            "statuses_count": hit.statuses_count,
        },
    })
}
