//! Conversation keys — epic 25.
//!
//! Every direct status carries a stored `conversation_key`: the
//! lowercase SHA-256 hex of the sorted, deduplicated participant ids
//! (author plus `status_mentions.account_id`s) serialized as ascending
//! decimal ids joined with `,`. The same set of participants therefore
//! always derives the same key — new replies in an exchange keep the
//! parent's key through the reply mention carry-over (`mentions::resolve`),
//! while a changed participant set moves the status to a new key.
//!
//! The key is opaque and carries no authority on its own: every
//! conversation API read/write re-verifies that the caller is a
//! participant (`src/http/api/v1/conversations.rs`).
//!
//! [`refresh_conversation_key`] keeps the column correct at every
//! `status_mentions` write site and must stay byte-for-byte identical
//! to the migration-0023 backfill expression — see the parity test in
//! `tests/e2e/conversations.rs`.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};

use super::{row_to_status, StatusRow, STATUS_COLS};
use crate::{http::paginate::Page, Result};

/// The conversation key for a participant set: sort ascending (numeric,
/// never lexical), dedupe, join decimal ids with `,`, SHA-256 hex.
/// `pub` — the e2e suite pins it against the migration-0023 backfill
/// expression.
pub fn participant_key(ids: &[i64]) -> String {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let joined = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    hex::encode(Sha256::digest(joined.as_bytes()))
}

/// Recompute `statuses.conversation_key` for `status_id` inside the
/// caller's transaction. Locks the status row so concurrent edits
/// cannot each compute from a stale mention set; sets the key for
/// `direct` statuses and clears it when an edit moved the status out
/// of `direct`.
pub(crate) async fn refresh_conversation_key(
    conn: &mut PgConnection,
    status_id: i64,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT visibility, account_id, conversation_key FROM statuses WHERE id = $1 FOR UPDATE",
    )
    .bind(status_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let visibility: String = row.get("visibility");
    if visibility != "direct" {
        // Skip the write when there's nothing to clear — this path
        // runs for every status write, not just direct ones.
        if row.get::<Option<String>, _>("conversation_key").is_some() {
            sqlx::query("UPDATE statuses SET conversation_key = NULL WHERE id = $1")
                .bind(status_id)
                .execute(&mut *conn)
                .await?;
        }
        return Ok(());
    }
    let mut ids: Vec<i64> =
        sqlx::query_scalar("SELECT account_id FROM status_mentions WHERE status_id = $1")
            .bind(status_id)
            .fetch_all(&mut *conn)
            .await?;
    ids.push(row.get("account_id"));
    let key = participant_key(&ids);
    sqlx::query("UPDATE statuses SET conversation_key = $2 WHERE id = $1")
        .bind(status_id)
        .bind(&key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Standalone form for callers not already inside a transaction
/// (account import): opens and commits its own transaction so the
/// `FOR UPDATE` lock actually serializes. `pub` — the e2e parity test
/// drives it directly.
pub async fn refresh_conversation_key_standalone(db: &PgPool, status_id: i64) -> Result<()> {
    let mut tx = db.begin().await?;
    refresh_conversation_key(&mut tx, status_id).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Read/write operations (epic 25; `http::api::v1::conversations`)
// ---------------------------------------------------------------------------

/// The caller participates in `key` when they authored or are named in
/// a `status_mentions` row of any non-tombstoned status carrying it.
/// Every handler runs this before reading or writing — a matching key
/// alone never grants access.
pub(crate) async fn is_participant(db: &PgPool, key: &str, caller: i64) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS( \
             SELECT 1 FROM statuses s \
             WHERE s.conversation_key = $1 AND s.visibility = 'direct' AND NOT s.tombstone \
               AND (s.account_id = $2 OR EXISTS ( \
                   SELECT 1 FROM status_mentions m \
                   WHERE m.status_id = s.id AND m.account_id = $2)))",
    )
    .bind(key)
    .bind(caller)
    .fetch_one(db)
    .await?)
}

/// One conversation group: the key and its newest visible status id
/// (the list cursor — grouped pagination orders by it).
pub(crate) struct ConversationGroup {
    pub key: String,
    pub newest_id: i64,
}

/// The caller's conversations, newest-visible-status first, grouped
/// **before** pagination so a page never splits a conversation in two.
/// Conversations the caller hid (`hidden_through_status_id` at/after
/// the newest visible status) drop out until a newer status revives
/// them for that caller only.
pub(crate) async fn list(db: &PgPool, caller: i64, page: &Page) -> Result<Vec<ConversationGroup>> {
    let rows = sqlx::query(
        "SELECT g.conversation_key, g.newest_id FROM ( \
             SELECT s.conversation_key, MAX(s.id) AS newest_id \
             FROM statuses s \
             WHERE s.conversation_key IS NOT NULL AND s.visibility = 'direct' AND NOT s.tombstone \
               AND (s.account_id = $1 OR EXISTS ( \
                   SELECT 1 FROM status_mentions m \
                   WHERE m.status_id = s.id AND m.account_id = $1)) \
             GROUP BY s.conversation_key \
         ) g \
         LEFT JOIN conversation_markers mk \
             ON mk.account_id = $1 AND mk.conversation_key = g.conversation_key \
         WHERE g.newest_id > COALESCE(mk.hidden_through_status_id, 0) \
           AND ($2::bigint IS NULL OR g.newest_id < $2) \
           AND ($3::bigint IS NULL OR g.newest_id > $3) \
         ORDER BY g.newest_id DESC \
         LIMIT $4",
    )
    .bind(caller)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ConversationGroup {
            key: r.get("conversation_key"),
            newest_id: r.get("newest_id"),
        })
        .collect())
}

/// Participants for a whole page of conversations at once. Returns
/// `key -> other participant ids` — the deduped union of authors and
/// mentioned accounts over every non-tombstoned status with each key
/// (account deletion can leave a participant in the key without rows —
/// the union reads what is actually there).
pub(crate) async fn other_participants_many(
    db: &PgPool,
    keys: &[String],
    caller: i64,
) -> Result<HashMap<String, Vec<i64>>> {
    let rows = sqlx::query(
        "SELECT p.conversation_key, p.account_id FROM ( \
             SELECT s.conversation_key, s.account_id FROM statuses s \
             WHERE s.conversation_key = ANY($1) AND NOT s.tombstone \
             UNION \
             SELECT s.conversation_key, m.account_id FROM status_mentions m \
             JOIN statuses s ON s.id = m.status_id \
             WHERE s.conversation_key = ANY($1) AND NOT s.tombstone \
         ) p \
         WHERE p.account_id <> $2",
    )
    .bind(keys)
    .bind(caller)
    .fetch_all(db)
    .await?;
    let mut map: HashMap<String, Vec<i64>> = HashMap::new();
    for r in &rows {
        map.entry(r.get::<String, _>("conversation_key"))
            .or_default()
            .push(r.get::<i64, _>("account_id"));
    }
    Ok(map)
}

/// Batch-load status rows by id — the conversation list's
/// `last_status` set in one query.
pub(crate) async fn statuses_by_id(db: &PgPool, ids: &[i64]) -> Result<HashMap<i64, StatusRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {STATUS_COLS} FROM statuses WHERE id = ANY($1)"
    ))
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = row_to_status(r);
            (s.id, s)
        })
        .collect())
}

/// The caller's markers for a set of keys — one read for the whole
/// conversation page.
pub(crate) async fn markers(
    db: &PgPool,
    caller: i64,
    keys: &[String],
) -> Result<HashMap<String, (Option<i64>, Option<i64>)>> {
    let rows = sqlx::query(
        "SELECT conversation_key, last_read_status_id, hidden_through_status_id \
         FROM conversation_markers \
         WHERE account_id = $1 AND conversation_key = ANY($2)",
    )
    .bind(caller)
    .bind(keys)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("conversation_key"),
                (
                    r.get::<Option<i64>, _>("last_read_status_id"),
                    r.get::<Option<i64>, _>("hidden_through_status_id"),
                ),
            )
        })
        .collect())
}

/// The thread's statuses, newest first, `Link`-paginated — tombstones
/// excluded; the caller's participation is checked by the handler.
pub(crate) async fn thread(
    db: &PgPool,
    caller: i64,
    key: &str,
    page: &Page,
) -> Result<Vec<StatusRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {STATUS_COLS} FROM statuses s \
         WHERE s.conversation_key = $1 AND s.visibility = 'direct' AND NOT s.tombstone \
           AND (s.account_id = $2 OR EXISTS ( \
               SELECT 1 FROM status_mentions m \
               WHERE m.status_id = s.id AND m.account_id = $2)) \
           AND ($3::bigint IS NULL OR s.id < $3) \
           AND ($4::bigint IS NULL OR s.id > $4) \
         ORDER BY s.id DESC LIMIT $5"
    ))
    .bind(key)
    .bind(caller)
    .bind(page.max_id)
    .bind(page.since_id)
    .bind(page.limit)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_status).collect())
}

/// Upsert the caller's read marker to the newest visible status id —
/// never touches other participants' rows.
pub(crate) async fn mark_read(db: &PgPool, caller: i64, key: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO conversation_markers (account_id, conversation_key, last_read_status_id) \
         SELECT $2, $1, MAX(s.id) FROM statuses s \
         WHERE s.conversation_key = $1 AND s.visibility = 'direct' AND NOT s.tombstone \
           AND (s.account_id = $2 OR EXISTS ( \
               SELECT 1 FROM status_mentions m \
               WHERE m.status_id = s.id AND m.account_id = $2)) \
         ON CONFLICT (account_id, conversation_key) \
         DO UPDATE SET last_read_status_id = EXCLUDED.last_read_status_id",
    )
    .bind(key)
    .bind(caller)
    .execute(db)
    .await?;
    Ok(())
}

/// Upsert the caller's hide marker to the newest status id with the
/// key (tombstones included — the marker covers the whole thread).
/// Shared statuses are never deleted; a newer status revives the
/// conversation for this caller only.
pub(crate) async fn hide(db: &PgPool, caller: i64, key: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO conversation_markers (account_id, conversation_key, hidden_through_status_id) \
         SELECT $2, $1, MAX(s.id) FROM statuses s \
         WHERE s.conversation_key = $1 \
         ON CONFLICT (account_id, conversation_key) \
         DO UPDATE SET hidden_through_status_id = EXCLUDED.hidden_through_status_id",
    )
    .bind(key)
    .bind(caller)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_sorts_dedupes_and_hashes() {
        // Out-of-order ids, a self-mention duplicate, and multi-digit
        // ids — ordering is numeric ("2,10,30", never "10,2,30").
        let key = participant_key(&[30, 2, 10, 2]);
        assert_eq!(key, hex::encode(Sha256::digest(b"2,10,30".as_slice())));
        assert_eq!(key.len(), 64);
        assert!(key
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn key_is_order_independent() {
        assert_eq!(participant_key(&[1, 2, 3]), participant_key(&[3, 1, 2]));
        assert_ne!(participant_key(&[1, 2]), participant_key(&[1, 2, 3]));
    }
}
