//! `custom_emojis` and `emoji_packs` rows plus storage writes.

use sqlx::{PgPool, Row};

use crate::{
    config::Config,
    media::{images::MediaKind, storage::MediaStore},
    Error, Result,
};

/// One `custom_emojis` row. `domain NULL` = local (uploaded here);
/// remote rows are cached copies with `remote_url` and no
/// `image_path` (served from the remote instance).
#[derive(Debug)]
pub struct EmojiRow {
    pub id: i64,
    pub shortcode: String,
    pub domain: Option<String>,
    pub image_path: Option<String>,
    pub remote_url: Option<String>,
    pub disabled: bool,
    pub visible_in_picker: bool,
    pub category: Option<String>,
    pub pack_id: Option<i64>,
}

pub(crate) fn row_to_emoji(r: &sqlx::postgres::PgRow) -> EmojiRow {
    EmojiRow {
        id: r.get("id"),
        shortcode: r.get("shortcode"),
        domain: r.get("domain"),
        image_path: r.get("image_path"),
        remote_url: r.get("remote_url"),
        disabled: r.get("disabled"),
        visible_in_picker: r.get("visible_in_picker"),
        category: r.get("category"),
        pack_id: r.get("pack_id"),
    }
}

const EMOJI_COLS: &str = "id, shortcode, domain, image_path, remote_url, \
     disabled, visible_in_picker, category, pack_id";

/// `category` length cap — bounds picker-grouping strings the way
/// `emoji_packs.name` is bounded (the column is plain `TEXT`).
const MAX_CATEGORY_LEN: usize = 100;

fn validate_category(c: &str) -> Result<()> {
    if c.chars().count() > MAX_CATEGORY_LEN {
        return Err(Error::Validation(format!(
            "emoji category must be at most {MAX_CATEGORY_LEN} characters"
        )));
    }
    Ok(())
}

/// `^[a-zA-Z0-9_]{2,100}$` — matches the schema CHECK (Rust is one
/// char stricter than the DB's `+` bound, documented).
pub fn validate_shortcode(code: &str) -> Result<String> {
    let code = code.trim_matches(':');
    if code.len() < 2 || code.len() > 100 {
        return Err(Error::Validation(
            "emoji shortcode must be 2-100 characters".to_string(),
        ));
    }
    if !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::Validation(format!(
            "invalid emoji shortcode ':{code}:' (letters, digits, underscores only)"
        )));
    }
    Ok(code.to_string())
}

/// Load one emoji by id.
pub async fn get(db: &PgPool, id: i64) -> Result<EmojiRow> {
    let row = sqlx::query(&format!(
        "SELECT {EMOJI_COLS} FROM custom_emojis WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;
    row.as_ref()
        .map(row_to_emoji)
        .ok_or_else(|| Error::NotFound("emoji".into()))
}

/// The local emoji row for `shortcode`, if any.
async fn local_by_shortcode(db: &PgPool, shortcode: &str) -> Result<Option<EmojiRow>> {
    let row = sqlx::query(&format!(
        "SELECT {EMOJI_COLS} FROM custom_emojis \
         WHERE shortcode = $1 AND domain IS NULL"
    ))
    .bind(shortcode)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_emoji))
}

/// [`local_by_shortcode`] with a row lock — runs inside
/// [`replace_local`]'s transaction so concurrent overwrites
/// serialise on the row instead of reading the same stale key.
async fn local_by_shortcode_for_update<'e, E>(db: E, shortcode: &str) -> Result<Option<EmojiRow>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(&format!(
        "SELECT {EMOJI_COLS} FROM custom_emojis \
         WHERE shortcode = $1 AND domain IS NULL FOR UPDATE"
    ))
    .bind(shortcode)
    .fetch_optional(db)
    .await?;
    Ok(row.as_ref().map(row_to_emoji))
}

/// Store the image and insert the row. `uploader_id` names the
/// storage namespace (`emoji/{account}/{uuid}.{ext}`); the row is
/// instance-owned. Duplicate local shortcode → `Error::Conflict`
/// (including concurrent creates racing past the pre-check — the
/// stored object is removed on the way out).
#[allow(clippy::too_many_arguments)]
pub async fn create_local(
    db: &PgPool,
    store: &MediaStore,
    config: &Config,
    shortcode: &str,
    bytes: Vec<u8>,
    uploader_id: i64,
    pack_id: Option<i64>,
    category: Option<&str>,
) -> Result<EmojiRow> {
    let shortcode = validate_shortcode(shortcode)?;
    if let Some(c) = category {
        validate_category(c)?;
    }
    if local_by_shortcode(db, &shortcode).await?.is_some() {
        return Err(Error::Conflict(format!(
            "emoji :{shortcode}: already exists"
        )));
    }
    if let Some(pack) = pack_id {
        get_pack(db, pack).await?;
    }
    let image = crate::media::images::store_profile_image(
        config,
        store,
        MediaKind::Emoji,
        uploader_id,
        bytes,
    )
    .await?;
    insert_or_cleanup(db, store, &shortcode, &image.key, pack_id, category).await
}

/// [`insert_local`] plus best-effort removal of the stored object
/// when the insert fails — a failed insert must not orphan bytes.
async fn insert_or_cleanup(
    db: &PgPool,
    store: &MediaStore,
    shortcode: &str,
    image_key: &str,
    pack_id: Option<i64>,
    category: Option<&str>,
) -> Result<EmojiRow> {
    match insert_local(db, shortcode, image_key, pack_id, category).await {
        Ok(row) => Ok(row),
        Err(e) => {
            if let Err(de) = store.delete(image_key).await {
                tracing::warn!(key = image_key, error = %de, "orphaned emoji object");
            }
            Err(e)
        }
    }
}

/// Non-destructive `overwrite` path for bulk imports: validate and
/// store the replacement *before* touching the row, then swap
/// `image_path` in place — the row id (and the `custom_emoji_id`
/// references on reactions) survive. With no existing row this is a
/// plain insert. Returns the row plus the old object's storage key
/// for the caller to delete once the swap has committed.
#[allow(clippy::too_many_arguments)]
pub async fn replace_local(
    db: &PgPool,
    store: &MediaStore,
    config: &Config,
    shortcode: &str,
    bytes: Vec<u8>,
    uploader_id: i64,
    pack_id: Option<i64>,
    category: Option<&str>,
) -> Result<(EmojiRow, Option<String>)> {
    let shortcode = validate_shortcode(shortcode)?;
    if let Some(c) = category {
        validate_category(c)?;
    }
    if let Some(pack) = pack_id {
        get_pack(db, pack).await?;
    }
    // The image pipeline is the validator — a bad file fails here,
    // leaving the existing emoji completely untouched.
    let image = crate::media::images::store_profile_image(
        config,
        store,
        MediaKind::Emoji,
        uploader_id,
        bytes,
    )
    .await?;
    // The swap runs in a transaction with the existing row locked
    // (`FOR UPDATE`): two concurrent overwrites of the same shortcode
    // serialise, so each reads the other's committed `image_path` and
    // deletes it — neither replacement is orphaned. With no existing
    // row the lock is a no-op and the unique index decides the race.
    let mut tx = db.begin().await?;
    let result: Result<(EmojiRow, Option<String>)> = async {
        match local_by_shortcode_for_update(&mut *tx, &shortcode).await? {
            Some(old) => {
                let row = sqlx::query(&format!(
                    "UPDATE custom_emojis SET image_path = $2, category = $3, pack_id = $4, \
                            disabled = false, visible_in_picker = true, updated_at = now() \
                     WHERE id = $1 RETURNING {EMOJI_COLS}"
                ))
                .bind(old.id)
                .bind(&image.key)
                .bind(category)
                .bind(pack_id)
                .fetch_one(&mut *tx)
                .await?;
                Ok((row_to_emoji(&row), old.image_path))
            }
            None => Ok((
                insert_local(&mut *tx, &shortcode, &image.key, pack_id, category).await?,
                None,
            )),
        }
    }
    .await;
    match result {
        Ok(done) => {
            tx.commit().await?;
            Ok(done)
        }
        Err(e) => {
            // The transaction rolls back on drop — remove the stored
            // replacement so it isn't orphaned.
            if let Err(de) = store.delete(&image.key).await {
                tracing::warn!(key = %image.key, error = %de, "orphaned emoji object");
            }
            Err(e)
        }
    }
}

/// Insert the row for already-stored image bytes (import paths reuse
/// the image pipeline through `store_profile_image` too). A unique-
/// index race with a concurrent create surfaces as `Error::Conflict`.
/// Runs on any executor so callers can embed it in a transaction.
pub async fn insert_local<'e, E>(
    db: E,
    shortcode: &str,
    image_path: &str,
    pack_id: Option<i64>,
    category: Option<&str>,
) -> Result<EmojiRow>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if let Some(c) = category {
        validate_category(c)?;
    }
    let row = sqlx::query(&format!(
        "INSERT INTO custom_emojis (shortcode, image_path, pack_id, category) \
         VALUES ($1,$2,$3,$4) RETURNING {EMOJI_COLS}"
    ))
    .bind(shortcode)
    .bind(image_path)
    .bind(pack_id)
    .bind(category)
    .fetch_one(db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::Conflict(format!("emoji :{shortcode}: already exists"))
        }
        e => e.into(),
    })?;
    Ok(row_to_emoji(&row))
}

/// Distinguishes a missing JSON field (`None` — leave untouched)
/// from an explicit `null` (`Some(None)` — clear the column).
/// Serde's default maps both to `None`, so `deserialize_with` is
/// required for tri-state patch fields.
fn double_option<'de, T, D>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// Fields an admin may patch — all optional. `category`/`pack_id`
/// are tri-state: absent keeps the current value, JSON `null`
/// clears it, a value sets it.
#[derive(Debug, Default, serde::Deserialize)]
pub struct EmojiPatch {
    pub shortcode: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub category: Option<Option<String>>,
    pub visible_in_picker: Option<bool>,
    pub disabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub pack_id: Option<Option<i64>>,
}

/// Partial update — local rows only (remote cached rows are
/// federation-managed, not admin-editable). `shortcode` renames
/// re-validate and re-check the local-unique index.
pub async fn update(db: &PgPool, id: i64, patch: EmojiPatch) -> Result<EmojiRow> {
    let row = get(db, id).await?;
    if row.domain.is_some() {
        return Err(Error::NotFound("emoji".into()));
    }
    if let Some(Some(c)) = &patch.category {
        validate_category(c)?;
    }
    let shortcode = match &patch.shortcode {
        Some(c) => validate_shortcode(c)?,
        None => row.shortcode.clone(),
    };
    if shortcode != row.shortcode {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM custom_emojis \
             WHERE shortcode = $1 AND COALESCE(domain,'') = COALESCE($2,'') \
               AND id <> $3)",
        )
        .bind(&shortcode)
        .bind(&row.domain)
        .bind(id)
        .fetch_one(db)
        .await?;
        if exists {
            return Err(Error::Conflict(format!(
                "emoji :{shortcode}: already exists"
            )));
        }
    }
    let category = patch.category.unwrap_or(row.category);
    let visible_in_picker = patch.visible_in_picker.unwrap_or(row.visible_in_picker);
    let disabled = patch.disabled.unwrap_or(row.disabled);
    let pack_id = patch.pack_id.unwrap_or(row.pack_id);
    if let Some(pack) = pack_id {
        get_pack(db, pack).await?;
    }
    let updated = sqlx::query(&format!(
        "UPDATE custom_emojis SET shortcode = $2, category = $3, \
                visible_in_picker = $4, disabled = $5, pack_id = $6, \
                updated_at = now() WHERE id = $1 RETURNING {EMOJI_COLS}"
    ))
    .bind(id)
    .bind(&shortcode)
    .bind(&category)
    .bind(visible_in_picker)
    .bind(disabled)
    .bind(pack_id)
    .fetch_one(db)
    .await?;
    Ok(row_to_emoji(&updated))
}

/// Delete a local row; the stored object is removed best-effort
/// (reaction rows keep working — `custom_emoji_id` goes NULL via
/// SET NULL). Remote cached rows are not admin-deletable.
pub async fn delete(db: &PgPool, store: &MediaStore, id: i64) -> Result<()> {
    let row = get(db, id).await?;
    if row.domain.is_some() {
        return Err(Error::NotFound("emoji".into()));
    }
    sqlx::query("DELETE FROM custom_emojis WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    if let Some(path) = &row.image_path {
        if let Err(e) = store.delete(path).await {
            tracing::warn!(path, error = %e, "emoji object delete failed");
        }
    }
    Ok(())
}

/// Admin listing — optional pack filter, `include_disabled` toggles
/// the disabled rows (admin views see everything).
pub async fn list_admin(
    db: &PgPool,
    pack_id: Option<i64>,
    include_disabled: bool,
) -> Result<Vec<EmojiRow>> {
    let mut sql = format!("SELECT {EMOJI_COLS} FROM custom_emojis WHERE domain IS NULL");
    if !include_disabled {
        sql.push_str(" AND NOT disabled");
    }
    if pack_id.is_some() {
        sql.push_str(" AND pack_id = $1");
    }
    sql.push_str(" ORDER BY shortcode");
    let mut q = sqlx::query(&sql);
    if let Some(p) = pack_id {
        q = q.bind(p);
    }
    let rows = q.fetch_all(db).await?;
    Ok(rows.iter().map(row_to_emoji).collect())
}

/// Enabled local emoji for the public list / render map.
pub async fn local_enabled(db: &PgPool) -> Result<Vec<EmojiRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {EMOJI_COLS} FROM custom_emojis \
         WHERE domain IS NULL AND NOT disabled ORDER BY shortcode"
    ))
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_emoji).collect())
}

/// Enabled emoji for one remote domain (inbound rendering).
pub async fn domain_enabled(db: &PgPool, domain: &str) -> Result<Vec<EmojiRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {EMOJI_COLS} FROM custom_emojis \
         WHERE domain = $1 AND NOT disabled ORDER BY shortcode"
    ))
    .bind(domain)
    .fetch_all(db)
    .await?;
    Ok(rows.iter().map(row_to_emoji).collect())
}

/// The URL check for remote emoji images — http(s) only. Anything
/// else (`data:`, `javascript:`, …) is rejected: the URL lands in an
/// `<img src>` served to every viewer.
pub fn valid_remote_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Upsert a remote `Emoji` AP tag: `domain` + `remote_url`, no
/// `image_path` (served from the remote URL). `name` may carry the
/// colons. Best-effort callers ignore errors.
pub async fn upsert_remote<'e, E>(
    db: E,
    shortcode: &str,
    domain: &str,
    remote_url: &str,
) -> Result<EmojiRow>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let shortcode = validate_shortcode(shortcode)?;
    if !valid_remote_url(remote_url) {
        return Err(Error::Validation(format!(
            "remote emoji URL for :{shortcode}: is not http(s)"
        )));
    }
    let row = sqlx::query(&format!(
        "INSERT INTO custom_emojis (shortcode, domain, remote_url) \
         VALUES ($1,$2,$3) \
         ON CONFLICT (shortcode, COALESCE(domain,'')) DO UPDATE \
         SET remote_url = EXCLUDED.remote_url, updated_at = now() \
         RETURNING {EMOJI_COLS}"
    ))
    .bind(&shortcode)
    .bind(domain)
    .bind(remote_url)
    .fetch_one(db)
    .await?;
    Ok(row_to_emoji(&row))
}

/// One `emoji_packs` row.
#[derive(Debug)]
pub struct PackRow {
    pub id: i64,
    pub name: String,
}

/// Load a pack — 404 for missing/foreign use.
pub async fn get_pack(db: &PgPool, id: i64) -> Result<PackRow> {
    let row = sqlx::query("SELECT id, name FROM emoji_packs WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?;
    row.map(|r| PackRow {
        id: r.get("id"),
        name: r.get("name"),
    })
    .ok_or_else(|| Error::NotFound("emoji_pack".into()))
}

/// Create a pack; duplicate name → `Error::Conflict`.
pub async fn create_pack(db: &PgPool, name: &str) -> Result<PackRow> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(Error::Validation("invalid pack name".to_string()));
    }
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM emoji_packs WHERE name = $1)")
            .bind(name)
            .fetch_one(db)
            .await?;
    if exists {
        return Err(Error::Conflict(format!(
            "emoji pack '{name}' already exists"
        )));
    }
    let row = sqlx::query("INSERT INTO emoji_packs (name) VALUES ($1) RETURNING id, name")
        .bind(name)
        .fetch_one(db)
        .await?;
    Ok(PackRow {
        id: row.get("id"),
        name: row.get("name"),
    })
}

/// Packs with emoji counts, name-sorted.
pub async fn list_packs(db: &PgPool) -> Result<Vec<(PackRow, i64)>> {
    let rows = sqlx::query(
        "SELECT p.id, p.name, COUNT(e.id)::bigint AS emoji_count \
         FROM emoji_packs p LEFT JOIN custom_emojis e ON e.pack_id = p.id \
         GROUP BY p.id, p.name ORDER BY p.name",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                PackRow {
                    id: r.get("id"),
                    name: r.get("name"),
                },
                r.get("emoji_count"),
            )
        })
        .collect())
}

/// Rename a pack.
pub async fn update_pack(db: &PgPool, id: i64, name: &str) -> Result<PackRow> {
    get_pack(db, id).await?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(Error::Validation("invalid pack name".to_string()));
    }
    let row = sqlx::query(
        "UPDATE emoji_packs SET name = $2, updated_at = now() \
         WHERE id = $1 RETURNING id, name",
    )
    .bind(id)
    .bind(name)
    .fetch_one(db)
    .await?;
    Ok(PackRow {
        id: row.get("id"),
        name: row.get("name"),
    })
}

/// Delete a pack — its emoji keep their rows with `pack_id NULL`.
pub async fn delete_pack(db: &PgPool, id: i64) -> Result<()> {
    let affected = sqlx::query("DELETE FROM emoji_packs WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected();
    if affected == 0 {
        return Err(Error::NotFound("emoji_pack".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_distinguishes_absent_from_null() {
        let p: EmojiPatch = serde_json::from_str("{}").unwrap();
        assert!(p.category.is_none() && p.pack_id.is_none());
        let p: EmojiPatch = serde_json::from_str(r#"{"category": null, "pack_id": null}"#).unwrap();
        assert!(matches!(p.category, Some(None)));
        assert!(matches!(p.pack_id, Some(None)));
        let p: EmojiPatch = serde_json::from_str(r#"{"category": "memes", "pack_id": 7}"#).unwrap();
        assert!(matches!(p.category, Some(Some(ref c)) if c == "memes"));
        assert!(matches!(p.pack_id, Some(Some(7))));
    }
}
