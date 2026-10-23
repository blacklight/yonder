//! Private user notes — `account_notes` upserts. The comment is only
//! ever joined into the *author's* Relationship render, so privacy is
//! structural (see `entity.rs`).

use sqlx::PgPool;

use crate::{Error, Result};

/// Set `caller_id`'s note on `target_id` — `""` clears the row
/// (Mastodon's `comment: ""` is `DELETE`-equivalent).
pub async fn set_note(db: &PgPool, caller_id: i64, target_id: i64, comment: &str) -> Result<()> {
    if comment.chars().count() > 2000 {
        return Err(Error::Validation("note is too long (2000 max)".into()));
    }
    if comment.is_empty() {
        sqlx::query("DELETE FROM account_notes WHERE account_id = $1 AND target_account_id = $2")
            .bind(caller_id)
            .bind(target_id)
            .execute(db)
            .await?;
    } else {
        sqlx::query(
            "INSERT INTO account_notes (account_id, target_account_id, comment) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (account_id, target_account_id) \
             DO UPDATE SET comment = EXCLUDED.comment, updated_at = now()",
        )
        .bind(caller_id)
        .bind(target_id)
        .bind(comment)
        .execute(db)
        .await?;
    }
    Ok(())
}
