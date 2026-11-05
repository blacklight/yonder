//! Archive restoration (epic 31 section 7): the indexed member set is
//! normalized and written per item — posts/boosts/collections — with
//! the two-pass graph fix-up and per-item failure accounting.

use super::import::*;
use super::*;
use crate::{activitypub, jobs::JobContext, Error, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::{PgConnection, PgPool};

// ---------------------------------------------------------------------------
// Archive import (indexed members → per-kind, then posts)
// ---------------------------------------------------------------------------

/// Sub-kinds inside an archive, in processing order (posts last).
/// `blocked_domains.csv`/`custom_filters.json` are the Mastodon names;
/// the old Yonder names stay accepted.
pub(crate) const ARCHIVE_KINDS: &[&str] = &[
    "following_accounts.csv",
    "blocked_accounts.csv",
    "muted_accounts.csv",
    "blocked_domains.csv",
    "domain_blocks.csv",
    "bookmarks.csv",
    "lists.csv",
    "custom_filters.json",
    "filters.json",
    "outbox.json",
    "likes.json",
    "bookmarks.json",
];

fn kind_for_member(name: &str) -> Option<Kind> {
    match name {
        "following_accounts.csv" => Some(Kind::Following),
        "blocked_accounts.csv" => Some(Kind::Blocking),
        "muted_accounts.csv" => Some(Kind::Muting),
        "blocked_domains.csv" | "domain_blocks.csv" => Some(Kind::DomainBlocks),
        "bookmarks.csv" => Some(Kind::Bookmarks),
        "lists.csv" => Some(Kind::Lists),
        "filters.json" | "custom_filters.json" => Some(Kind::Filters),
        _ => None,
    }
}

/// What happened to one normalized item; the [`Tally`] keeps the
/// categories independent (contract §7.6).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ItemOutcome {
    /// Newly written.
    Applied,
    /// Newly written, but this many attachments were unavailable (their
    /// bytes were refused and no absolute reference survived).
    Partial(i64),
    /// Dedupe hit — the source identity already maps locally.
    Present,
    /// Intentionally not imported (e.g. an `Announce` with no object
    /// URI). A status whose attachment validation fails still lands —
    /// its unavailable media is reported as `Partial`.
    Skipped,
    /// A type the engine does not handle (non-Create/Announce, or an
    /// unsupported Create object type).
    Unsupported,
}

/// Extract `orderedItems`/`items` from an OrderedCollection, or a bare
/// array (old Yonder archives). Malformed supported members are an
/// error, never a silent zero.
fn collection_items(bytes: &[u8], member: &str) -> Result<Vec<Value>> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| Error::Validation(format!("archive member {member} is malformed: {e}")))?;
    match &v {
        Value::Array(items) => Ok(items.clone()),
        // An object must carry one of the collection keys — a `{}`
        // outbox is malformed, never a silent zero-item collection.
        Value::Object(_) => match v.get("orderedItems").or_else(|| v.get("items")) {
            Some(Value::Array(items)) => Ok(items.clone()),
            _ => Err(Error::Validation(format!(
                "archive member {member} is not a collection"
            ))),
        },
        _ => Err(Error::Validation(format!(
            "archive member {member} is not a collection"
        ))),
    }
}

/// URI strings out of a collection — strings or objects with `id`.
fn uri_items(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.get("id").and_then(|i| i.as_str()).map(String::from))
        })
        .collect()
}

/// Strip LD-signature/proof blocks and per-item `@context` (pre-4.2
/// archives) — user-supplied evidence, not authorization.
fn strip_envelope(item: &mut Value) {
    if let Some(obj) = item.as_object_mut() {
        obj.remove("@context");
        obj.remove("signature");
        obj.remove("proof");
    }
    if let Some(obj) = item.get_mut("object").and_then(Value::as_object_mut) {
        obj.remove("@context");
        obj.remove("signature");
        obj.remove("proof");
    }
}

/// Normalize Mastodon's `custom_filters.json` entries (`filter_action`,
/// `keywords_attributes`, `statuses_attributes`) plus legacy `filters`
/// wrappers to the internal shape [`apply_filter`] consumes.
fn normalize_filter_doc(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut entries = parse_filters(bytes)?;
    for e in &mut entries {
        if let Some(obj) = e.as_object_mut() {
            if let Some(action) = obj.remove("filter_action") {
                obj.entry("action").or_insert(action);
            }
            if let Some(kw) = obj.remove("keywords_attributes") {
                obj.entry("keywords").or_insert(kw);
            }
            obj.remove("statuses_attributes");
            obj.remove("id");
        }
    }
    Ok(entries)
}

/// Drive an archive import: cursor = `kind_index * KIND_OFFSET + row`.
pub(crate) async fn import_archive(run: ImportRun<'_>) -> Result<()> {
    let ImportRun {
        ctx,
        rel,
        import_id,
        account_id,
        mode,
        file_key,
        cursor,
        bytes,
        options,
        lease,
        deadline,
        ..
    } = run;
    let federate = options
        .get("federate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && rel.ap.is_some();
    // Decompression of a full-size archive is CPU-bound — it runs on
    // the blocking pool, never on an async worker.
    let indexed = tokio::task::spawn_blocking(move || {
        archive::index(&bytes).map(archive::ArchiveIndex::strip_wrapper)
    })
    .await
    .map_err(|e| Error::Internal(format!("archive index task: {e}")))?;
    let index = match indexed {
        Ok(i) => i,
        Err(e) => {
            return fail_import(ctx, import_id, lease, file_key, &e.to_string()).await;
        }
    };
    // A recognizable archive imports at least one member — a `done`
    // row with zero items is a silent-zero lie, so this fails loudly.
    if !ARCHIVE_KINDS.iter().any(|kind| index.contains(kind)) {
        return fail_import(
            ctx,
            import_id,
            lease,
            file_key,
            "archive has no importable members",
        )
        .await;
    }
    let mut kind_idx = cursor / KIND_OFFSET;
    let mut offset = cursor % KIND_OFFSET;
    let mut tally = Tally::default();

    while (kind_idx as usize) < ARCHIVE_KINDS.len() {
        let member = ARCHIVE_KINDS[kind_idx as usize];
        let content = match index.get(member) {
            Some(b) => b,
            None => {
                kind_idx += 1;
                offset = 0;
                continue;
            }
        };

        if member == "outbox.json" {
            let items = match collection_items(content, member) {
                Ok(i) => i,
                Err(e) => {
                    return fail_import(ctx, import_id, lease, file_key, &e.to_string()).await;
                }
            };
            let mut idx = offset as usize;
            while idx < items.len()
                && tally.seen() < BATCH_ROWS
                && std::time::Instant::now() < deadline
            {
                let mut item = items[idx].clone();
                strip_envelope(&mut item);
                match import_outbox_item(ctx, Some(&index), import_id, account_id, &item, federate)
                    .await
                {
                    Ok(o) => record(&mut tally, o),
                    Err(e) => {
                        tally.failed += 1;
                        tracing::debug!(import_id, row = idx, error = %e, "post import failed");
                    }
                }
                idx += 1;
            }
            offset = idx as i64;
            if idx < items.len() {
                break;
            }
        } else if member == "likes.json" || member == "bookmarks.json" {
            let items = match collection_items(content, member) {
                Ok(i) => uri_items(&i),
                Err(e) => {
                    return fail_import(ctx, import_id, lease, file_key, &e.to_string()).await;
                }
            };
            let favourite = member == "likes.json";
            let mut idx = offset as usize;
            while idx < items.len()
                && tally.seen() < BATCH_ROWS
                && std::time::Instant::now() < deadline
            {
                match apply_collection_item(ctx, account_id, import_id, &items[idx], favourite)
                    .await
                {
                    Ok(o) => record(&mut tally, o),
                    Err(e) => {
                        tally.failed += 1;
                        tracing::debug!(import_id, member, row = idx, error = %e, "collection import failed");
                    }
                }
                idx += 1;
            }
            offset = idx as i64;
            if idx < items.len() {
                break;
            }
        } else if member == "custom_filters.json" || member == "filters.json" {
            let entries = match normalize_filter_doc(content) {
                Ok(e) => e,
                Err(e) => {
                    return fail_import(ctx, import_id, lease, file_key, &e.to_string()).await;
                }
            };
            if mode == "overwrite" && offset == 0 {
                sqlx::query("DELETE FROM filters WHERE account_id = $1")
                    .bind(account_id)
                    .execute(&ctx.db)
                    .await?;
            }
            let mut idx = offset as usize;
            while idx < entries.len()
                && tally.seen() < BATCH_ROWS
                && std::time::Instant::now() < deadline
            {
                match apply_filter(&ctx.db, account_id, &entries[idx]).await {
                    Ok(()) => tally.applied += 1,
                    Err(_) => tally.failed += 1,
                }
                idx += 1;
            }
            offset = idx as i64;
            if idx < entries.len() {
                break;
            }
        } else if let Some(kind) = kind_for_member(member) {
            let rows = match parse_rows(kind, content) {
                Ok(r) => r,
                Err(e) => {
                    return fail_import(ctx, import_id, lease, file_key, &e.to_string()).await;
                }
            };
            overwrite_prep(
                ctx, rel, account_id, kind, &rows, mode, offset, import_id, lease,
            )
            .await?;
            let mut idx = offset as usize;
            while idx < rows.len()
                && tally.seen() < BATCH_ROWS
                && std::time::Instant::now() < deadline
            {
                match apply_row(ctx, rel, account_id, import_id, kind, &rows[idx]).await {
                    Ok(()) => tally.applied += 1,
                    Err(e) => {
                        tally.failed += 1;
                        tracing::debug!(import_id, member, row = idx, error = %e, "import row failed");
                    }
                }
                idx += 1;
            }
            offset = idx as i64;
            if idx < rows.len() {
                break;
            }
        }
        kind_idx += 1;
        offset = 0;
    }

    let cursor = kind_idx * KIND_OFFSET + offset;
    if (kind_idx as usize) < ARCHIVE_KINDS.len() {
        return requeue_import(ctx, import_id, lease, file_key, cursor, &tally).await;
    }
    if !persist_progress(ctx, import_id, lease, file_key, cursor, &tally).await? {
        return Ok(());
    }
    // Two-pass fix-up: replies/quotes resolve through the source-ID
    // mapping regardless of archive order (imported self-copies win,
    // original remote URIs still match for other authors).
    fix_up_graph(&ctx.db, account_id).await?;
    // §8: enqueue the drain *before* committing `done` — a crash or
    // enqueue failure here keeps the import `running` so the retry
    // re-enqueues it; work rows can never be stranded.
    if federate {
        crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs)
            .enqueue(
                crate::jobs::JobKind::ImportPublish,
                json!({ "import_id": import_id }),
            )
            .await?;
    }
    finish_import(ctx, import_id, lease, file_key).await
}

fn record(tally: &mut Tally, outcome: ItemOutcome) {
    match outcome {
        ItemOutcome::Applied => tally.applied += 1,
        ItemOutcome::Partial(unavailable) => {
            tally.applied += 1;
            tally.media_unavailable += unavailable;
        }
        ItemOutcome::Present => tally.present += 1,
        ItemOutcome::Skipped => tally.skipped += 1,
        ItemOutcome::Unsupported => tally.unsupported += 1,
    }
}

/// Resolve self-replies and quotes after all items landed. Self-copies
/// win through the per-account `original_id`; other authors match the
/// canonical remote `uri` only — never another importer's copy.
pub(crate) async fn fix_up_graph(db: &PgPool, account_id: i64) -> Result<()> {
    for (col, link, extra) in [
        (
            "in_reply_to_id",
            "in_reply_to_uri",
            ", in_reply_to_account_id = p.account_id",
        ),
        ("quote_of_id", "quote_uri", ""),
    ] {
        sqlx::query(&format!(
            "UPDATE statuses c SET {col} = p.id{extra} \
             FROM statuses p \
             WHERE c.account_id = $1 AND c.{col} IS NULL AND c.{link} IS NOT NULL \
               AND p.account_id = c.account_id AND p.original_id = c.{link}",
        ))
        .bind(account_id)
        .execute(db)
        .await?;
        sqlx::query(&format!(
            "UPDATE statuses c SET {col} = p.id{extra} \
             FROM statuses p \
             WHERE c.account_id = $1 AND c.{col} IS NULL AND c.{link} IS NOT NULL \
               AND p.uri = c.{link}",
        ))
        .bind(account_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// A favourites/bookmarks row on a possibly-remote object: the status
/// is stubbed by URI when unknown locally. Self-posts map to the
/// imported copy through `original_id` before the canonical `uri`.
pub(crate) async fn apply_collection_item(
    ctx: &JobContext,
    account_id: i64,
    import_id: i64,
    uri: &str,
    favourite: bool,
) -> Result<ItemOutcome> {
    if remote_reference(uri).is_none() {
        return Err(Error::Validation(format!(
            "collection item {uri:?} is not an absolute http(s) URI"
        )));
    }
    let mut tx = ctx.db.begin().await?;
    // The importer's own copy wins — a like/bookmark on their own
    // post attaches to the imported row, not a remote stub.
    let status_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE (account_id = $1 AND original_id = $2) OR uri = $2 \
         ORDER BY (account_id = $1 AND original_id = $2) DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(uri)
    .fetch_optional(&mut *tx)
    .await?;
    let (status_id, stubbed) = match status_id {
        Some(id) => (id, false),
        None => ensure_remote_status(&mut tx, uri).await?,
    };
    let n = sqlx::query(&format!(
        "INSERT INTO {} (account_id, status_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        if favourite { "favourites" } else { "bookmarks" }
    ))
    .bind(account_id)
    .bind(status_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    record_item(
        &mut tx,
        import_id,
        account_id,
        if favourite { "like" } else { "bookmark" },
        uri,
        Some(status_id),
        if n > 0 { "done" } else { "skipped" },
        None,
        None,
        None,
    )
    .await?;
    tx.commit().await?;
    if stubbed {
        request_status_fetch(ctx, uri).await;
    }
    Ok(if n > 0 {
        ItemOutcome::Applied
    } else {
        ItemOutcome::Present
    })
}

/// Upsert the `import_items` bookkeeping row (source→destination
/// mapping, contract §2.4). Retries and replays coalesce on the
/// `(account, source_uri, kind)` unique key.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_item(
    tx: &mut PgConnection,
    import_id: i64,
    account_id: i64,
    kind: &str,
    source_uri: &str,
    target_status_id: Option<i64>,
    state: &str,
    in_reply_to_uri: Option<&str>,
    quote_uri: Option<&str>,
    reblog_uri: Option<&str>,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO import_items (import_id, account_id, kind, source_uri, \
                                   target_status_id, state, in_reply_to_uri, \
                                   quote_uri, reblog_uri) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (account_id, source_uri, kind) DO UPDATE SET \
           import_id = EXCLUDED.import_id, \
           target_status_id = COALESCE(EXCLUDED.target_status_id, import_items.target_status_id), \
           state = EXCLUDED.state, updated_at = now() \
         RETURNING id",
    )
    .bind(import_id)
    .bind(account_id)
    .bind(kind)
    .bind(source_uri)
    .bind(target_status_id)
    .bind(state)
    .bind(in_reply_to_uri)
    .bind(quote_uri)
    .bind(reblog_uri)
    .fetch_one(&mut *tx)
    .await?;
    Ok(id)
}

/// A `publication_work` row for one authored item (contract §8): the
/// durable bridge between the committed status and the delivery
/// enqueue — deterministic `activity_id` makes retries once-only.
async fn queue_publication(
    tx: &mut PgConnection,
    import_id: i64,
    import_item_id: i64,
    status_id: i64,
    status_uri: &str,
    activity_type: &str,
    username: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO publication_work (import_id, import_item_id, status_id, \
                                       activity_type, activity_id, payload) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (activity_id) DO NOTHING",
    )
    .bind(import_id)
    .bind(import_item_id)
    .bind(status_id)
    .bind(activity_type)
    .bind(format!("{status_uri}#imports/{import_id}/{activity_type}"))
    .bind(json!({ "status_id": status_id, "username": username }))
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Enqueue a `RemoteStatusFetch` for a remote URI an import only
/// references — deduped by the shared marker, best-effort (a failed
/// enqueue only means the object stays unresolved until something
/// else fetches it).
async fn request_status_fetch(ctx: &JobContext, uri: &str) {
    let dispatcher = crate::jobs::Dispatcher::new(&ctx.redis, &ctx.config.jobs);
    if let Err(e) =
        crate::posts::remote::enqueue_status_fetch_deduped(&ctx.redis, &dispatcher, uri).await
    {
        tracing::warn!(uri, error = %e, "remote status fetch enqueue failed");
    }
}

/// Find or stub a remote status row by URI (like/bookmark targets the
/// archive references but does not embed). Stubs stay `direct` — they
/// are placeholders, not listable public content. Returns the id and
/// whether a *stub* was created — the caller enqueues a
/// `RemoteStatusFetch` for stubs so their content is hydrated instead
/// of rendering as an empty card.
async fn ensure_remote_status(db: &mut PgConnection, uri: &str) -> Result<(i64, bool)> {
    if let Some(id) = sqlx::query_scalar::<_, i64>("SELECT id FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_optional(&mut *db)
        .await?
    {
        return Ok((id, false));
    }
    let domain = url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "remote.invalid".to_string());
    let actor_uri = format!("https://{domain}/accounts/_unknown");
    let account_id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1, '_unknown', $2, $2, '') ON CONFLICT (actor_id) DO UPDATE SET id = accounts.id \
         RETURNING id",
    )
    .bind(&domain)
    .bind(&actor_uri)
    .fetch_one(&mut *db)
    .await?;
    let id = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri, url, visibility, local) \
         VALUES ($1, '', $2, $2, 'direct', false) \
         ON CONFLICT (uri) DO UPDATE SET id = statuses.id RETURNING id",
    )
    .bind(account_id)
    .bind(uri)
    .fetch_one(&mut *db)
    .await?;
    Ok((id, true))
}

/// Map an AP audience to `(to, cc)` — the inverse of
/// [`visibility_from_audience`].
pub(crate) fn visibility_audience(
    visibility: &str,
    followers_url: &str,
) -> (Vec<String>, Vec<String>) {
    match visibility {
        "public" => (vec![AS_PUBLIC.to_string()], vec![followers_url.to_string()]),
        "unlisted" => (vec![followers_url.to_string()], vec![AS_PUBLIC.to_string()]),
        "private" | "followers_only" => (vec![followers_url.to_string()], vec![]),
        _ => (vec![], vec![]),
    }
}

/// Derive local visibility from an object's `to`/`cc`.
fn visibility_from_audience(obj: &Value) -> &'static str {
    let to = str_list(obj.get("to"));
    let cc = str_list(obj.get("cc"));
    let public = |l: &[String]| l.iter().any(|v| v == AS_PUBLIC);
    let followers = |l: &[String]| l.iter().any(|v| v.ends_with("/followers"));
    if public(&to) {
        "public"
    } else if public(&cc) {
        "unlisted"
    } else if followers(&to) || followers(&cc) {
        "private"
    } else {
        "direct"
    }
}

/// JSON string-list field → `Vec<String>` (single strings wrap).
/// Shared with `moves` (alsoKnownAs parsing).
pub(crate) fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn id_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str().or_else(|| v.get("id").and_then(|i| i.as_str())))
        .map(String::from)
}

/// Dispatch one normalized outbox item: `Create` Note/Question →
/// authored status (or DM history); `Announce` → boost row.
/// `index` is `None` for API-sourced items (no embedded media).
pub(crate) async fn import_outbox_item(
    ctx: &JobContext,
    index: Option<&archive::ArchiveIndex>,
    import_id: i64,
    account_id: i64,
    item: &Value,
    federate: bool,
) -> Result<ItemOutcome> {
    match item.get("type").and_then(Value::as_str) {
        Some("Create") => import_post(ctx, index, import_id, account_id, item, federate).await,
        Some("Announce") => import_boost(ctx, import_id, account_id, item, federate).await,
        _ => Ok(ItemOutcome::Unsupported),
    }
}

/// Import an `Announce` as a boost of the (possibly stubbed) original.
/// The Announce's own id/timestamp stay provenance on the boost row;
/// its audience sets the boost's visibility.
async fn import_boost(
    ctx: &JobContext,
    import_id: i64,
    account_id: i64,
    item: &Value,
    federate: bool,
) -> Result<ItemOutcome> {
    let db = &ctx.db;
    let Some(object_uri) = id_str(item.get("object")) else {
        return Ok(ItemOutcome::Skipped);
    };
    let announce_id = id_str(item.get("id")).unwrap_or_else(|| object_uri.clone());
    let published = item
        .get("published")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let visibility = visibility_from_audience(item);

    let mut tx = db.begin().await?;
    // The importer's own copy wins through `original_id`, then the
    // canonical remote `uri` — the same precedence collection items
    // use. Only a *materialized* hit boosts synchronously; a stub or
    // unknown target parks in `pending_boosts` until the remote object
    // lands, so unreachable targets never render as empty boost cards.
    let target: Option<(i64, bool)> = sqlx::query_as(
        "SELECT id, (COALESCE(content, '') <> '' OR object_type IS NOT NULL) \
         FROM statuses \
         WHERE NOT tombstone \
           AND ((account_id = $1 AND original_id = $2) OR uri = $2) \
         ORDER BY (account_id = $1 AND original_id = $2) DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(&object_uri)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((target_id, true)) = target else {
        return defer_boost(
            ctx,
            tx,
            import_id,
            account_id,
            &object_uri,
            &announce_id,
            published,
            visibility,
            federate,
        )
        .await;
    };
    // A boost that already reached us over federation carries no
    // `original_id`, so the dedupe arbiter below never matches it —
    // `uix_statuses_reblog` fires instead, and the raw 23505 would
    // retry the page forever. Presence on either key means the
    // Announce is already represented.
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE account_id = $1 AND NOT tombstone \
         AND (reblog_of_id = $2 OR original_id = $3))",
    )
    .bind(account_id)
    .bind(target_id)
    .bind(&announce_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        tx.rollback().await?;
        return Ok(ItemOutcome::Present);
    }
    let boost_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, content, visibility, local, reblog_of_id, \
                               original_id, original_created_at, created_at) \
         VALUES (timestamp_id('statuses', COALESCE($5, now())), \
                 $1, '', $6, true, $2, $3, $4, $4) \
         ON CONFLICT (account_id, original_id) WHERE original_id IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(account_id)
    .bind(target_id)
    .bind(&announce_id)
    .bind(published)
    .bind(published)
    .bind(visibility)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(boost_id) = boost_id else {
        tx.rollback().await?;
        return Ok(ItemOutcome::Present);
    };
    let (actor_id, username): (String, String) =
        sqlx::query_as("SELECT actor_id, username FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
    let boost_uri = format!("{actor_id}/statuses/{boost_id}");
    sqlx::query("UPDATE statuses SET uri = $2 WHERE id = $1")
        .bind(boost_id)
        .bind(&boost_uri)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE statuses SET reblogs_count = reblogs_count + 1 WHERE id = $1")
        .bind(target_id)
        .execute(&mut *tx)
        .await?;
    let item_id = record_item(
        &mut tx,
        import_id,
        account_id,
        "boost",
        &announce_id,
        Some(boost_id),
        "done",
        None,
        None,
        Some(&object_uri),
    )
    .await?;
    // `federate` never bulk re-sends non-public audiences (contract
    // §8.3): only public/unlisted boosts get durable work.
    if federate && matches!(visibility, "public" | "unlisted") {
        queue_publication(
            &mut tx, import_id, item_id, boost_id, &boost_uri, "Announce", &username,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(ItemOutcome::Applied)
}

/// Park an Announce whose target isn't materialized: the boost row is
/// written by [`backfill_pending_boosts`] when the remote object lands
/// (fetch job, inbound `Create`, outbox sync). The `original_id` half
/// of the dedupe still applies now; the `reblog_of_id` half waits for
/// the resolved target id.
#[allow(clippy::too_many_arguments)]
async fn defer_boost(
    ctx: &JobContext,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    import_id: i64,
    account_id: i64,
    object_uri: &str,
    announce_id: &str,
    published: DateTime<Utc>,
    visibility: &str,
    federate: bool,
) -> Result<ItemOutcome> {
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM statuses WHERE account_id = $1 AND NOT tombstone \
         AND original_id = $2)",
    )
    .bind(account_id)
    .bind(announce_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        tx.rollback().await?;
        return Ok(ItemOutcome::Present);
    }
    sqlx::query(
        "INSERT INTO pending_boosts (account_id, target_uri, announce_id, published, \
                                     visibility, import_id, federate) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
    )
    .bind(account_id)
    .bind(object_uri)
    .bind(announce_id)
    .bind(published)
    .bind(visibility)
    .bind(import_id)
    .bind(federate)
    .execute(&mut *tx)
    .await?;
    record_item(
        &mut tx,
        import_id,
        account_id,
        "boost",
        announce_id,
        None,
        "pending",
        None,
        None,
        Some(object_uri),
    )
    .await?;
    tx.commit().await?;
    request_status_fetch(ctx, object_uri).await;
    Ok(ItemOutcome::Applied)
}

/// Materialize parked boost intents for a just-landed remote status —
/// the boost counterpart of `posts::remote::backfill_pending_quotes`.
/// Runs on every path that resolves a remote URI to a status row
/// (inbound federation, `RemoteStatusFetch`, outbox sync), so a
/// deferred import boost appears whether hydration came from our own
/// fetch or a delivered `Create`. `uri` is the *requested* URI — the
/// fetched doc's canonical `id` may differ, which is why `status_id`
/// (the resolved row) is passed separately.
pub async fn backfill_pending_boosts(
    db: &PgPool,
    dispatcher: &crate::jobs::Dispatcher,
    status_id: i64,
    uri: &str,
) -> Result<()> {
    /// `(id, account_id, announce_id, published, visibility, import_id, federate)`
    type PendingBoostRow = (i64, i64, String, DateTime<Utc>, String, i64, bool);
    let pending: Vec<PendingBoostRow> = sqlx::query_as(
        "SELECT id, account_id, announce_id, published, visibility, import_id, federate \
             FROM pending_boosts WHERE target_uri = $1",
    )
    .bind(uri)
    .fetch_all(db)
    .await?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut publish_imports: Vec<i64> = Vec::new();
    for (pending_id, account_id, announce_id, published, visibility, import_id, federate) in pending
    {
        let mut tx = db.begin().await?;
        // The target id is known now — re-run the full dedupe: a
        // boost may have landed via another path while parked.
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM statuses WHERE account_id = $1 AND NOT tombstone \
             AND (reblog_of_id = $2 OR original_id = $3) LIMIT 1",
        )
        .bind(account_id)
        .bind(status_id)
        .bind(&announce_id)
        .fetch_optional(&mut *tx)
        .await?;
        let mut created = false;
        let boost_id: Option<i64> = match existing {
            Some(id) => Some(id),
            None => {
                let id: Option<i64> = sqlx::query_scalar(
                    "INSERT INTO statuses (id, account_id, content, visibility, local, \
                                           reblog_of_id, original_id, original_created_at, \
                                           created_at) \
                     VALUES (timestamp_id('statuses', COALESCE($5, now())), \
                             $1, '', $6, true, $2, $3, $4, $4) \
                     ON CONFLICT (account_id, original_id) WHERE original_id IS NOT NULL \
                     DO NOTHING RETURNING id",
                )
                .bind(account_id)
                .bind(status_id)
                .bind(&announce_id)
                .bind(published)
                .bind(published)
                .bind(&visibility)
                .fetch_optional(&mut *tx)
                .await?;
                match id {
                    Some(id) => {
                        created = true;
                        Some(id)
                    }
                    // A concurrent writer won the `original_id` slot —
                    // adopt its row.
                    None => {
                        sqlx::query_scalar(
                            "SELECT id FROM statuses WHERE account_id = $1 AND NOT tombstone \
                         AND original_id = $2 LIMIT 1",
                        )
                        .bind(account_id)
                        .bind(&announce_id)
                        .fetch_optional(&mut *tx)
                        .await?
                    }
                }
            }
        };
        let Some(boost_id) = boost_id else {
            tx.rollback().await?;
            continue;
        };
        // URI, counters and publication bookkeeping only move for the
        // row this drain actually inserted — an adopted/existing boost
        // was already accounted for when it landed.
        let mut publication: Option<(String, String)> = None;
        if created {
            let (actor_id, username): (String, String) =
                sqlx::query_as("SELECT actor_id, username FROM accounts WHERE id = $1")
                    .bind(account_id)
                    .fetch_one(&mut *tx)
                    .await?;
            let boost_uri = format!("{actor_id}/statuses/{boost_id}");
            sqlx::query("UPDATE statuses SET uri = $2 WHERE id = $1")
                .bind(boost_id)
                .bind(&boost_uri)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
                .bind(account_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE statuses SET reblogs_count = reblogs_count + 1 WHERE id = $1")
                .bind(status_id)
                .execute(&mut *tx)
                .await?;
            publication = Some((boost_uri, username));
        }
        let item_id = record_item(
            &mut tx,
            import_id,
            account_id,
            "boost",
            &announce_id,
            Some(boost_id),
            "done",
            None,
            None,
            Some(uri),
        )
        .await?;
        // `federate` never bulk re-sends non-public audiences (contract
        // §8.3): only public/unlisted boosts get durable work.
        if let Some((boost_uri, username)) = publication {
            if federate && matches!(visibility.as_str(), "public" | "unlisted") {
                queue_publication(
                    &mut tx, import_id, item_id, boost_id, &boost_uri, "Announce", &username,
                )
                .await?;
                if !publish_imports.contains(&import_id) {
                    publish_imports.push(import_id);
                }
            }
        }
        sqlx::query("DELETE FROM pending_boosts WHERE id = $1")
            .bind(pending_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    // A drain can land after the import's own `ImportPublish` run —
    // enqueue a fresh drain so late publication_work isn't stranded.
    for import_id in publish_imports {
        if let Err(e) = dispatcher
            .enqueue(
                crate::jobs::JobKind::ImportPublish,
                json!({ "import_id": import_id }),
            )
            .await
        {
            tracing::warn!(import_id, error = %e, "deferred boost publication enqueue failed");
        }
    }
    Ok(())
}

/// Mark `import_items` bookkeeping failed for parked boosts whose
/// target fetch settled permanently. The `pending_boosts` row stays —
/// a later materialization (marker TTL expiry, an inbound `Create`)
/// can still land the boost and flip the item back to `done`.
pub(crate) async fn fail_pending_boosts(db: &PgPool, uri: &str) -> Result<()> {
    sqlx::query(
        "UPDATE import_items i SET state = 'failed', updated_at = now() \
         FROM pending_boosts p \
         WHERE p.target_uri = $1 AND i.account_id = p.account_id \
           AND i.kind = 'boost' AND i.source_uri = p.announce_id \
           AND i.state = 'pending'",
    )
    .bind(uri)
    .execute(db)
    .await?;
    Ok(())
}

/// Validate embedded attachment bytes against their magic signature
/// — never the archive's self-description alone. Returns the stored
/// `media_attachments` `(type, content_type, extension)` triple, or
/// `None` for bytes the status-media pipeline would also refuse
/// (markup, scripts, opaque blobs). Order matters: images first so
/// ISOBMFF still formats (HEIC/AVIF) aren't mislabeled video; the
/// declared type is only an A/V fallback, subtype-checked like the
/// upload path.
fn validated_embedded(bytes: &[u8], declared: &str) -> Option<(String, String, String)> {
    if let Ok(fmt) = image::guess_format(bytes) {
        let (ext, mime) = match fmt {
            image::ImageFormat::Png => ("png", "image/png"),
            image::ImageFormat::Jpeg => ("jpg", "image/jpeg"),
            image::ImageFormat::WebP => ("webp", "image/webp"),
            image::ImageFormat::Gif => ("gif", "image/gif"),
            _ => return None,
        };
        return Some(("image".to_string(), mime.to_string(), ext.to_string()));
    }
    // HEIC/HEIF/AVIF still images share the ISOBMFF `ftyp` box the
    // A/V sniffer would call video.
    if let Some(brand) = bytes.get(4..12) {
        if &brand[..4] == b"ftyp" {
            let (ext, mime) = match &brand[4..] {
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => ("heic", "image/heic"),
                b"avif" | b"avis" => ("avif", "image/avif"),
                _ => ("", ""),
            };
            if !ext.is_empty() {
                return Some(("image".to_string(), mime.to_string(), ext.to_string()));
            }
        }
    }
    if let Some((ext, mime, kind)) = crate::media::attachments::sniff_media(bytes) {
        return Some((kind.to_string(), mime.to_string(), ext.to_string()));
    }
    // Declared A/V fallback — `for_declared` validates the subtype
    // exactly like the upload path. Everything else (markup,
    // octet-stream blobs, key material) is refused and the
    // attachment stays a remote reference.
    crate::media::attachments::for_declared(declared)
        .map(|(ext, mime, kind)| (kind.to_string(), mime, ext))
}

/// Remote references only ever keep absolute `http(s)` URLs — an
/// archive-relative member path (a refused embedded member) or an
/// arbitrary scheme is never fetchable, so the row is dropped rather
/// than stored as a broken link clients can be sent to.
fn remote_reference(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .map(|_| url.to_string())
}

/// A plausible `type/subtype` hint for a remote-reference row, or the
/// empty string (mapped to NULL on insert) — archive-declared types
/// are hints only, never stored verbatim.
fn declared_content_type(s: &str) -> String {
    let t = s.split(';').next().unwrap_or("").trim();
    let ok = t.split_once('/').is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.'))
            && b.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.'))
    });
    if ok {
        t.to_string()
    } else {
        String::new()
    }
}

/// A staged attachment row: `file_path`/`remote_url`, `type`,
/// `content_type`, `description` — written inside the item's
/// transaction after any embedded bytes are put to storage.
type StagedMedia = (
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
);

/// Import one `Create` item from an outbox: an authored status when
/// it had a public/followers audience, an owner-only private history
/// record when it was a direct message. Never federates; deduped on
/// `(account_id, original_id)`. All DB writes for the item share one
/// transaction so a crash cannot leave a dedupe-keyed partial row.
async fn import_post(
    ctx: &JobContext,
    index: Option<&archive::ArchiveIndex>,
    import_id: i64,
    account_id: i64,
    item: &Value,
    federate: bool,
) -> Result<ItemOutcome> {
    let db = &ctx.db;
    let obj = item.get("object").cloned().unwrap_or(Value::Null);
    let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(obj_type, "Note" | "Question") {
        return Ok(ItemOutcome::Unsupported);
    }
    let original_id =
        id_str(obj.get("id")).ok_or_else(|| Error::Validation("Create object lacks id".into()))?;
    // Attribution consistency: an activity whose actor does not match
    // the object's attributedTo is not the importer's own work, so it
    // is never imported under their identity (plan §E).
    if let (Some(actor), Some(attributed)) =
        (id_str(item.get("actor")), id_str(obj.get("attributedTo")))
    {
        if actor != attributed {
            return Ok(ItemOutcome::Unsupported);
        }
    }
    let account_row: (String, String) =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    let (username, actor_id) = account_row;

    let published = obj
        .get("published")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc));
    let source_html = obj
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("contentMap")
                .and_then(Value::as_object)
                .and_then(|m| m.values().next())
                .and_then(Value::as_str)
        })
        .unwrap_or("")
        .chars()
        .take(20_000)
        .collect::<String>();
    // AP `content` is HTML: searchable text goes to `content`,
    // sanitized HTML to `content_html` — never raw AP HTML alone.
    let content = crate::posts::render::html_to_text(&source_html);
    let content_html = crate::posts::render::sanitize_html(&source_html, &ctx.config);
    let summary = obj
        .get("summary")
        .and_then(Value::as_str)
        .map(|s| s.chars().take(2000).collect::<String>());
    let language = obj
        .get("contentMap")
        .and_then(Value::as_object)
        .and_then(|m| m.keys().next().cloned());
    let visibility = visibility_from_audience(&obj);
    let in_reply_to_uri = id_str(obj.get("inReplyTo"));
    let quote_uri = id_str(obj.get("quoteUrl")).or_else(|| id_str(obj.get("quoteUri")));

    // Skip the whole item (media staging included) when the dedupe
    // key already maps to a row — the `ON CONFLICT`s in the
    // transactions below stay as the race-proof backstop.
    let already: bool = if visibility == "direct" {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM private_history_messages \
             WHERE owner_id = $1 AND source_uri = $2)",
        )
    } else {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM statuses \
             WHERE account_id = $1 AND original_id = $2)",
        )
    }
    .bind(account_id)
    .bind(&original_id)
    .fetch_one(db)
    .await?;
    if already {
        return Ok(ItemOutcome::Present);
    }

    // Remote-side aggregate counters are provenance, not live
    // interactions — they ride the `remote_*_count` columns (0042)
    // which render as GREATEST(local, remote) without inflating the
    // transactional counters.
    let remote_likes = item
        .get("yonder_likes_count")
        .and_then(Value::as_i64)
        .map(|n| n.clamp(0, i32::MAX as i64) as i32);
    let remote_shares = item
        .get("yonder_shares_count")
        .and_then(Value::as_i64)
        .map(|n| n.clamp(0, i32::MAX as i64) as i32);
    let remote_quotes = item
        .get("yonder_quotes_count")
        .and_then(Value::as_i64)
        .map(|n| n.clamp(0, i32::MAX as i64) as i32);

    // Stage attachments: embedded bytes are validated and put to
    // storage first (the row insert happens inside the item
    // transaction; a failed commit leaves only an orphan object,
    // never a dangling row).
    let mut media_rows: Vec<StagedMedia> = Vec::new();
    let mut unavailable = 0;
    for att in obj
        .get("attachment")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(url) = id_str(att.get("url")) else {
            continue;
        };
        let declared = att
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        let description = att
            .get("name")
            .and_then(Value::as_str)
            .map(|s| s.chars().take(5000).collect::<String>());
        match stage_attachment(ctx, index, account_id, &url, declared, description).await? {
            Some(row) => media_rows.push(row),
            None => unavailable += 1,
        }
    }

    if visibility == "direct" {
        let dm = DmItem {
            item,
            obj: &obj,
            source_uri: &original_id,
            original_at: published,
            body_html: &content_html,
            media_rows,
        };
        return import_dm(ctx, import_id, account_id, dm)
            .await
            .map(|o| with_unavailable(o, unavailable));
    }

    let mut tx = db.begin().await?;
    // Dedupe — one imported copy per source object.
    let status_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, content, content_html, spoiler_text, sensitive, language, \
                               visibility, local, original_id, original_created_at, created_at, \
                               in_reply_to_uri, quote_uri, \
                               remote_likes_count, remote_shares_count, remote_quotes_count) \
         VALUES (timestamp_id('statuses', COALESCE($8, now())), \
                 $1, $2, $3, $4, $5, $6, $7, true, $9, $8, $8, $10, $11, $12, $13, $14) \
         ON CONFLICT (account_id, original_id) WHERE original_id IS NOT NULL DO NOTHING \
         RETURNING id",
    )
    .bind(account_id)
    .bind(&content)
    .bind(&content_html)
    .bind(&summary)
    .bind(
        obj.get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
    .bind(&language)
    .bind(visibility)
    .bind(published)
    .bind(&original_id)
    .bind(&in_reply_to_uri)
    .bind(&quote_uri)
    .bind(remote_likes)
    .bind(remote_shares)
    .bind(remote_quotes)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(status_id) = status_id else {
        tx.rollback().await?;
        return Ok(ItemOutcome::Present);
    };
    // Mint the local URI from the new id (Mastodon's /statuses/{id}
    // shape under the actor) and a profile URL.
    let status_uri = format!("{actor_id}/statuses/{status_id}");
    sqlx::query("UPDATE statuses SET uri = $2, url = $3 WHERE id = $1")
        .bind(status_id)
        .bind(&status_uri)
        .bind(format!(
            "{}/@{}/{}",
            activitypub::federation_base_url(&ctx.config),
            username,
            status_id
        ))
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE accounts SET statuses_count = statuses_count + 1 WHERE id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;

    // Immediate reply linkage when the parent already exists — the
    // end-of-run fix-up pass resolves out-of-order parents too. The
    // `original_id` match is account-scoped (self-copies only); other
    // authors resolve through the canonical remote `uri`.
    if let Some(parent) = &in_reply_to_uri {
        sqlx::query(
            "UPDATE statuses SET in_reply_to_id = p.id, in_reply_to_account_id = p.account_id \
             FROM statuses p WHERE statuses.id = $1 \
               AND ((p.account_id = $3 AND p.original_id = $2) OR p.uri = $2)",
        )
        .bind(status_id)
        .bind(parent)
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    }

    // Hashtags and mentions.
    if let Some(tags) = obj.get("tag").and_then(Value::as_array) {
        for tag in tags {
            match tag.get("type").and_then(Value::as_str) {
                Some("Hashtag") => {
                    let name = tag
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .chars()
                        .take(200)
                        .collect::<String>();
                    if name.is_empty() {
                        continue;
                    }
                    let tag_id: i64 = sqlx::query_scalar(
                        "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (lower(name)) \
                         DO UPDATE SET name = EXCLUDED.name RETURNING id",
                    )
                    .bind(&name)
                    .fetch_one(&mut *tx)
                    .await?;
                    sqlx::query(
                        "INSERT INTO status_tags (status_id, tag_id) \
                         VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    )
                    .bind(status_id)
                    .bind(tag_id)
                    .execute(&mut *tx)
                    .await?;
                }
                Some("Mention") => {
                    let Some(href) = id_str(tag.get("href")) else {
                        continue;
                    };
                    // Mentioned accounts resolve best-effort; a bare
                    // upsert on the URI keeps the row without a fetch.
                    // Targetless DO NOTHING also swallows a
                    // (domain, username) collision — the mention is
                    // dropped rather than aborting the import tx.
                    sqlx::query(
                        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
                         VALUES ($1,$2,$3,$3,'') ON CONFLICT DO NOTHING",
                    )
                    .bind(activitypub::moderation::actor_domain(&href))
                    .bind(
                        href.trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or(&href)
                            .to_string(),
                    )
                    .bind(&href)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(
                        "INSERT INTO status_mentions (status_id, account_id) \
                         SELECT $1, id FROM accounts WHERE actor_id = $2 ON CONFLICT DO NOTHING",
                    )
                    .bind(status_id)
                    .bind(&href)
                    .execute(&mut *tx)
                    .await?;
                }
                _ => {}
            }
        }
    }

    // Poll (Question) — closed polls keep their tallies as history
    // without inventing individual votes.
    if obj_type == "Question" {
        let multiple = obj.get("anyOf").is_some();
        let options = obj
            .get("anyOf")
            .or_else(|| obj.get("oneOf"))
            .and_then(Value::as_array);
        if let Some(options) = options {
            let expires_at = obj
                .get("endTime")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));
            let poll_id: i64 = sqlx::query_scalar(
                "INSERT INTO polls (status_id, expires_at, multiple) VALUES ($1, $2, $3) \
                 RETURNING id",
            )
            .bind(status_id)
            .bind(expires_at)
            .bind(multiple)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE statuses SET poll_id = $2 WHERE id = $1")
                .bind(status_id)
                .bind(poll_id)
                .execute(&mut *tx)
                .await?;
            for (i, opt) in options.iter().enumerate() {
                let title = opt
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(500)
                    .collect::<String>();
                let votes: i64 = opt
                    .get("replies")
                    .and_then(|r| r.get("totalItems").and_then(Value::as_i64))
                    .unwrap_or(0);
                sqlx::query(
                    "INSERT INTO poll_options (poll_id, idx, title, votes_count) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT (poll_id, idx) DO NOTHING",
                )
                .bind(poll_id)
                .bind(i as i32)
                .bind(&title)
                .bind(votes)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    for (file_path, remote_url, media_type, content_type, description) in &media_rows {
        let ct = if content_type.is_empty() {
            None
        } else {
            Some(content_type.as_str())
        };
        sqlx::query(
            "INSERT INTO media_attachments (account_id, status_id, type, file_path, remote_url, \
             content_type, description, processed) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(account_id)
        .bind(status_id)
        .bind(media_type)
        .bind(file_path)
        .bind(remote_url)
        .bind(ct)
        .bind(description)
        .bind(file_path.is_some())
        .execute(&mut *tx)
        .await?;
    }
    let item_id = record_item(
        &mut tx,
        import_id,
        account_id,
        if in_reply_to_uri.is_some() {
            "reply"
        } else {
            "post"
        },
        &original_id,
        Some(status_id),
        "done",
        in_reply_to_uri.as_deref(),
        quote_uri.as_deref(),
        None,
    )
    .await?;
    // §8: only public/unlisted audiences may bulk re-send — private
    // and followers-only posts never leave through this toggle.
    if federate && matches!(visibility, "public" | "unlisted") {
        queue_publication(
            &mut tx,
            import_id,
            item_id,
            status_id,
            &status_uri,
            "Create",
            &username,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(with_unavailable(ItemOutcome::Applied, unavailable))
}

/// Downgrade an applied item to `Partial` when attachments were lost.
fn with_unavailable(outcome: ItemOutcome, unavailable: i64) -> ItemOutcome {
    match outcome {
        ItemOutcome::Applied if unavailable > 0 => ItemOutcome::Partial(unavailable),
        other => other,
    }
}

/// Stage one attachment as a `media_attachments` row: embedded bytes
/// that validate are stored, otherwise an absolute http(s) URL is kept
/// as a reference. `None` when neither survives.
async fn stage_attachment(
    ctx: &JobContext,
    index: Option<&archive::ArchiveIndex>,
    account_id: i64,
    url: &str,
    declared: &str,
    description: Option<String>,
) -> Result<Option<StagedMedia>> {
    if let Some(bytes) = index.and_then(|i| i.get(url)) {
        if let Some(row) =
            stage_embedded(ctx, account_id, bytes, declared, description.clone()).await?
        {
            return Ok(Some(row));
        }
    }
    let media_type = match declared {
        t if t.starts_with("image/") => "image",
        t if t.starts_with("video/") => "video",
        t if t.starts_with("audio/") => "audio",
        _ => "document",
    };
    Ok(remote_reference(url).map(|remote| {
        (
            None,
            Some(remote),
            media_type.to_string(),
            declared_content_type(declared),
            description,
        )
    }))
}

/// Validate embedded bytes and store them content-addressed. Allowlisted
/// images get the upload pipeline's bounded decode and (config-gated)
/// EXIF strip; `None` when the bytes are refused.
async fn stage_embedded(
    ctx: &JobContext,
    account_id: i64,
    bytes: &[u8],
    declared: &str,
    description: Option<String>,
) -> Result<Option<StagedMedia>> {
    let staged = match validated_embedded(bytes, declared) {
        Some((kind, _, ext))
            if kind == "image" && matches!(ext.as_str(), "png" | "jpg" | "webp" | "gif") =>
        {
            let raw = bytes.to_vec();
            let strip = ctx.config.media.strip_exif;
            match tokio::task::spawn_blocking(move || {
                crate::media::images::process_image(&raw, strip, "imported image")
            })
            .await
            {
                Ok(Ok(p)) => Some((kind, p.bytes, p.content_type, p.ext.to_string())),
                _ => None,
            }
        }
        Some((kind, mime, ext)) => Some((kind, bytes.to_vec(), mime, ext)),
        None => None,
    };
    let Some((kind, data, mime, ext)) = staged else {
        return Ok(None);
    };
    // Content-addressed keys over the stored bytes: a replay re-puts
    // identical bytes under the same key (a no-op), and two source
    // items can never overwrite each other.
    let digest = hex::encode(&sha2::Sha256::digest(&data)[..8]);
    let key = format!("imports/{account_id}/{digest}.{ext}");
    ctx.media.put(&key, &data, &mime).await?;
    Ok(Some((Some(key), None, kind, mime, description)))
}

/// Everything [`import_dm`] needs from the normalized `Create` —
/// bundled so the writer stays readable without an argument-count
/// lint waiver.
struct DmItem<'a> {
    item: &'a Value,
    obj: &'a Value,
    source_uri: &'a str,
    original_at: Option<DateTime<Utc>>,
    body_html: &'a str,
    media_rows: Vec<StagedMedia>,
}

/// A direct/private Create becomes an owner-only history row, never a
/// status — the original audience cannot be represented safely, and
/// the record is attributed to its original sender (contract §7.5).
async fn import_dm(
    ctx: &JobContext,
    import_id: i64,
    account_id: i64,
    dm: DmItem<'_>,
) -> Result<ItemOutcome> {
    let sender_uri = id_str(dm.obj.get("attributedTo"))
        .or_else(|| id_str(dm.item.get("actor")))
        .unwrap_or_default();
    let mut recipients = str_list(dm.obj.get("to"));
    recipients.extend(str_list(dm.obj.get("cc")));
    // Provenance for every attachment: stored keys plus the remote
    // references that stayed references.
    let media_keys: Vec<String> = dm
        .media_rows
        .iter()
        .filter_map(|r| r.0.clone().or_else(|| r.1.clone()))
        .collect();
    let source_origin = url::Url::parse(dm.source_uri)
        .ok()
        .and_then(|u| u.host_str().map(String::from));

    let mut tx = ctx.db.begin().await?;
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO private_history_messages \
           (owner_id, import_id, sender_uri, sender_account_id, recipient_uris, \
            body, media_keys, source_uri, source_origin, original_at) \
         VALUES ($1, $2, $3, \
                 (SELECT id FROM accounts WHERE actor_id = $3), \
                 $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (owner_id, source_uri) DO NOTHING RETURNING id",
    )
    .bind(account_id)
    .bind(import_id)
    .bind(&sender_uri)
    .bind(json!(recipients))
    .bind(dm.body_html)
    .bind(json!(media_keys))
    .bind(dm.source_uri)
    .bind(&source_origin)
    .bind(dm.original_at)
    .fetch_optional(&mut *tx)
    .await?;
    record_item(
        &mut tx,
        import_id,
        account_id,
        "dm",
        dm.source_uri,
        None,
        if inserted.is_some() {
            "done"
        } else {
            "skipped"
        },
        None,
        None,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(if inserted.is_some() {
        ItemOutcome::Applied
    } else {
        ItemOutcome::Present
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Embedded bytes are accepted on their signature, never the
    /// archive's declared type alone.
    #[test]
    fn embedded_media_requires_a_real_signature() {
        // Declared image but not image bytes → refused.
        assert!(validated_embedded(b"<script>x()</script>", "image/png").is_none());
        // Real PNG signature → stored as image/png.
        assert_eq!(
            validated_embedded(b"\x89PNG\r\n\x1a\n rest-of-file", "image/png"),
            Some((
                "image".to_string(),
                "image/png".to_string(),
                "png".to_string()
            ))
        );
        // Image formats status media rejects stay remote references.
        assert!(validated_embedded(b"BM\x00\x00\x00\x00", "image/bmp").is_none());
        // ISOBMFF still images are images, not video.
        assert_eq!(
            validated_embedded(b"\x00\x00\x00\x18ftypheic\x00\x00", "image/heic"),
            Some((
                "image".to_string(),
                "image/heic".to_string(),
                "heic".to_string()
            ))
        );
        // A/V the sniffer doesn't recognize rides on the validated
        // declared type.
        assert_eq!(
            validated_embedded(b"junk", "video/x-custom"),
            Some((
                "video".to_string(),
                "video/x-custom".to_string(),
                "mp4".to_string()
            ))
        );
        // Opaque blobs are never stored — no blind octet-stream
        // fallback (key material under a harmless name included).
        assert!(validated_embedded(b"junk", "application/octet-stream").is_none());
        assert!(
            validated_embedded(b"-----BEGIN PRIVATE KEY-----", "application/octet-stream")
                .is_none()
        );
        // Markup/script never embeds.
        assert!(validated_embedded(b"<html></html>", "text/html").is_none());
    }
}
