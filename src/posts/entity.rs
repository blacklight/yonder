//! Mastodon entity JSON — Status, ScheduledStatus, Poll and
//! MediaAttachment — plus the `yonder` extension block (raw `source`,
//! `content_type`, quote data) the PROMPT asks for.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use sqlx::Row;

use crate::{media, users::account as account_mapper, Result};

use super::{wire_visibility, PostContext, StatusRow, STATUS_COLS};

/// A `media_attachments` row.
#[derive(Debug)]
pub struct MediaRow {
    pub id: i64,
    pub account_id: i64,
    pub status_id: Option<i64>,
    pub kind: String,
    pub file_path: Option<String>,
    pub remote_url: Option<String>,
    pub preview_remote_url: Option<String>,
    pub content_type: Option<String>,
    pub file_size: Option<i64>,
    pub description: Option<String>,
    pub blurhash: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub focus_x: Option<f32>,
    pub focus_y: Option<f32>,
    pub processed: bool,
}

fn media_from_row(r: &sqlx::postgres::PgRow) -> MediaRow {
    MediaRow {
        id: r.get("id"),
        account_id: r.get("account_id"),
        status_id: r.get("status_id"),
        kind: r.get("type"),
        file_path: r.get("file_path"),
        remote_url: r.get("remote_url"),
        preview_remote_url: r.get("preview_remote_url"),
        content_type: r.get("content_type"),
        file_size: r.get("file_size"),
        description: r.get("description"),
        blurhash: r.get("blurhash"),
        width: r.get("width"),
        height: r.get("height"),
        focus_x: r.get("focus_x"),
        focus_y: r.get("focus_y"),
        processed: r.get("processed"),
    }
}

const MEDIA_COLS: &str = "id, account_id, status_id, type, file_path, remote_url, \
     preview_remote_url, content_type, \
     file_size, description, blurhash, width, height, focus_x, focus_y, processed";

/// Load one media row by id.
pub async fn load_media(ctx: &PostContext, media_id: i64) -> Result<Option<MediaRow>> {
    let row = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments WHERE id = $1"
    ))
    .bind(media_id)
    .fetch_optional(&ctx.db)
    .await?;
    Ok(row.as_ref().map(media_from_row))
}

/// Load every media row attached to a status.
pub async fn media_for_status(ctx: &PostContext, status_id: i64) -> Result<Vec<MediaRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments WHERE status_id = $1 ORDER BY id"
    ))
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows.iter().map(media_from_row).collect())
}

/// Media rows reserved by a scheduled status.
pub async fn media_for_scheduled(ctx: &PostContext, scheduled_id: i64) -> Result<Vec<MediaRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {MEDIA_COLS} FROM media_attachments m \
         JOIN scheduled_status_media sm ON sm.media_attachment_id = m.id \
         WHERE sm.scheduled_status_id = $1 ORDER BY m.id"
    ))
    .bind(scheduled_id)
    .fetch_all(&ctx.db)
    .await?;
    Ok(rows.iter().map(media_from_row).collect())
}

/// The public URL for a stored attachment (local key or remote URL).
pub fn media_url(config: &crate::config::Config, base_url: &str, row: &MediaRow) -> Option<String> {
    let local = row
        .file_path
        .as_ref()
        .map(|k| media::public_url(config, base_url, k));
    let remote = row.remote_url.clone();
    // Remote-media caching on → the local copy wins; off → the remote
    // URL is served directly.
    if config.media.fetch_remote_media {
        local.or(remote)
    } else {
        remote.or(local)
    }
}

/// The Mastodon MediaAttachment entity: `url`/`preview_url` resolve
/// the stored key to a public URL; `meta` carries dimensions and the
/// `focus` point when set.
pub fn media_entity(config: &crate::config::Config, base_url: &str, row: &MediaRow) -> Value {
    let url = media_url(config, base_url, row);
    let mut meta = json!({});
    if let (Some(w), Some(h)) = (row.width, row.height) {
        meta["original"] = json!({
            "width": w,
            "height": h,
            "size": format!("{w}x{h}"),
            "aspect": w as f64 / h.max(1) as f64,
        });
    }
    if let (Some(x), Some(y)) = (row.focus_x, row.focus_y) {
        meta["focus"] = json!({ "x": x, "y": y });
    }
    // Poster art (remote `image`/`icon`/`preview`) is the real
    // `preview_url` when present — it's what powers cover art in
    // audio/video players; otherwise the attachment URL itself stands
    // in (images thumbnail themselves).
    json!({
        "id": row.id.to_string(),
        "type": row.kind,
        "url": url,
        "preview_url": row.preview_remote_url.clone().or(url),
        "remote_url": row.remote_url,
        "description": row.description,
        "blurhash": row.blurhash,
        "meta": meta,
        "yonder": {
            "content_type": row.content_type,
            "file_size": row.file_size,
            "processed": row.processed,
        },
    })
}

/// Everything [`StatusBatch::render`] needs, fetched once per page in
/// a few `try_join!` waves instead of ~8 sequential queries per row —
/// on a high-RTT database link the per-row lookups dominate request
/// latency (a 20-status page issued ~150 round-trips).
///
/// Maps are keyed by status id unless noted. One level of reblog/quote
/// originals is included (`originals`/`visible`), matching the depth
/// bound the old recursive render applied.
struct StatusBatch {
    /// Status authors (incl. originals' authors) by `accounts.id` —
    /// the `account::load` projection per id.
    accounts: HashMap<i64, account_mapper::AccountRow>,
    /// `moved` entity per author id.
    moved: HashMap<i64, Value>,
    /// Emoji map per author domain — present only for domains some
    /// rendered text could reference (the `EmojiMap::for_author_texts`
    /// short-circuit, applied once per domain).
    emoji_maps: HashMap<Option<String>, crate::emoji::render::EmojiMap>,
    empty_map: crate::emoji::render::EmojiMap,
    /// Non-tombstoned reblog/quote originals by status id.
    originals: HashMap<i64, StatusRow>,
    /// Ids of `originals` the viewer may see — `posts::can_see` batched.
    visible: HashSet<i64>,
    media: HashMap<i64, Vec<MediaRow>>,
    /// `mentions[]` entity values per status.
    mentions: HashMap<i64, Vec<Value>>,
    /// Tag names per status (entity shape needs `base_url` at render).
    tags: HashMap<i64, Vec<String>>,
    /// First `preview_cards` row per status, as the `card` entity.
    cards: HashMap<i64, Value>,
    /// Poll entity values by `polls.id`.
    polls: HashMap<i64, Value>,
    /// Pleroma reaction summary per status.
    reactions: HashMap<i64, Vec<Value>>,
    /// `(account_id, status_id)` pairs present in `status_pins`.
    pinned: HashSet<(i64, i64)>,
    /// status id → `(favourited, bookmarked, reblogged)` for the viewer.
    flags: HashMap<i64, (bool, bool, bool)>,
}

impl StatusBatch {
    async fn load(ctx: &PostContext, rows: &[StatusRow], viewer: Option<i64>) -> Result<Self> {
        use sqlx::postgres::PgRow;
        let db = &ctx.db;
        let base_url = ctx.base_url();
        let to_err = |e: sqlx::Error| crate::Error::from(e);

        // Reblog/quote originals render one level deep — load them up
        // front. Children are fetched over rows ∪ referenced ids;
        // missing or tombstoned originals simply produce no rows.
        let mut nested_ids = Vec::new();
        for s in rows {
            if let Some(id) = s.reblog_of_id {
                nested_ids.push(id);
            }
            if let Some(id) = s.quote_of_id {
                nested_ids.push(id);
            }
        }
        nested_ids.sort_unstable();
        nested_ids.dedup();
        let mut all_ids: Vec<i64> = rows.iter().map(|s| s.id).collect();
        all_ids.extend_from_slice(&nested_ids);
        all_ids.sort_unstable();
        all_ids.dedup();

        // ── Wave 1: everything keyed only by status ids / viewer ──
        let originals_f = async {
            if nested_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query(&format!(
                "SELECT {STATUS_COLS} FROM statuses WHERE id = ANY($1) AND NOT tombstone"
            ))
            .bind(&nested_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let media_f = async {
            sqlx::query(&format!(
                "SELECT {MEDIA_COLS} FROM media_attachments \
                 WHERE status_id = ANY($1) ORDER BY status_id, id"
            ))
            .bind(&all_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let mentions_f = async {
            sqlx::query(
                "SELECT sm.status_id, a.id, a.username, a.domain, a.url, a.actor_id \
                 FROM status_mentions sm JOIN accounts a ON a.id = sm.account_id \
                 WHERE sm.status_id = ANY($1) ORDER BY sm.status_id, a.id",
            )
            .bind(&all_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let tags_f = async {
            sqlx::query_as::<_, (i64, String)>(
                "SELECT st.status_id, t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
                 WHERE st.status_id = ANY($1) ORDER BY st.status_id, t.name",
            )
            .bind(&all_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let cards_f = async {
            sqlx::query(
                "SELECT status_id, url, title, description, image_path, author_name, author_url, \
                        provider_name, provider_url FROM preview_cards \
                 WHERE status_id = ANY($1) ORDER BY status_id, id",
            )
            .bind(&all_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let reactions_f = async {
            sqlx::query_as::<_, (i64, String, Option<i64>, i64, Option<bool>)>(
                "SELECT status_id, emoji, custom_emoji_id, count(*)::bigint, \
                        CASE WHEN $2::bigint IS NULL THEN NULL \
                             ELSE bool_or(account_id = $2) END AS me \
                 FROM emoji_reactions WHERE status_id = ANY($1) \
                 GROUP BY status_id, emoji, custom_emoji_id \
                 ORDER BY status_id, min(created_at)",
            )
            .bind(&all_ids)
            .bind(viewer)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let faved_f = async {
            match viewer {
                Some(v) => sqlx::query_scalar::<_, i64>(
                    "SELECT status_id FROM favourites \
                     WHERE account_id = $1 AND status_id = ANY($2)",
                )
                .bind(v)
                .bind(&all_ids)
                .fetch_all(db)
                .await
                .map_err(to_err),
                None => Ok(Vec::new()),
            }
        };
        let marked_f = async {
            match viewer {
                Some(v) => sqlx::query_scalar::<_, i64>(
                    "SELECT status_id FROM bookmarks \
                     WHERE account_id = $1 AND status_id = ANY($2)",
                )
                .bind(v)
                .bind(&all_ids)
                .fetch_all(db)
                .await
                .map_err(to_err),
                None => Ok(Vec::new()),
            }
        };
        let boosted_f = async {
            match viewer {
                Some(v) => sqlx::query_scalar::<_, i64>(
                    "SELECT reblog_of_id FROM statuses WHERE account_id = $1 \
                     AND reblog_of_id = ANY($2) AND NOT tombstone",
                )
                .bind(v)
                .bind(&all_ids)
                .fetch_all(db)
                .await
                .map_err(to_err),
                None => Ok(Vec::new()),
            }
        };
        let (
            orig_rows,
            media_rows,
            mention_rows,
            tag_rows,
            card_rows,
            reaction_rows,
            faved,
            marked,
            boosted,
        ) = tokio::try_join!(
            originals_f,
            media_f,
            mentions_f,
            tags_f,
            cards_f,
            reactions_f,
            faved_f,
            marked_f,
            boosted_f
        )?;

        let originals: HashMap<i64, StatusRow> = orig_rows
            .iter()
            .map(|r: &PgRow| {
                let s = super::row_to_status(r);
                (s.id, s)
            })
            .collect();

        // A boost's inner status may itself carry a quote — its target
        // renders inside the reblog card (the one place a second-hop
        // original is reachable), so those rows and their per-status
        // projection join the batch too. Bounded at this one extra hop:
        // a quote inside a quote never renders.
        let grand_ids: Vec<i64> = originals
            .values()
            .filter_map(|o| o.quote_of_id)
            .filter(|id| !originals.contains_key(id) && !all_ids.contains(id))
            .collect();
        let mut originals = originals;
        let (
            mut media_rows,
            mut mention_rows,
            mut tag_rows,
            mut card_rows,
            mut reaction_rows,
            mut faved,
            mut marked,
            mut boosted,
        ) = (
            media_rows,
            mention_rows,
            tag_rows,
            card_rows,
            reaction_rows,
            faved,
            marked,
            boosted,
        );
        if !grand_ids.is_empty() {
            let grand_statuses_f = async {
                sqlx::query(&format!(
                    "SELECT {STATUS_COLS} FROM statuses WHERE id = ANY($1) AND NOT tombstone"
                ))
                .bind(&grand_ids)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_media_f = async {
                sqlx::query(&format!(
                    "SELECT {MEDIA_COLS} FROM media_attachments \
                     WHERE status_id = ANY($1) ORDER BY status_id, id"
                ))
                .bind(&grand_ids)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_mentions_f = async {
                sqlx::query(
                    "SELECT sm.status_id, a.id, a.username, a.domain, a.url, a.actor_id \
                     FROM status_mentions sm JOIN accounts a ON a.id = sm.account_id \
                     WHERE sm.status_id = ANY($1) ORDER BY sm.status_id, a.id",
                )
                .bind(&grand_ids)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_tags_f = async {
                sqlx::query_as::<_, (i64, String)>(
                    "SELECT st.status_id, t.name FROM status_tags st JOIN tags t ON t.id = st.tag_id \
                     WHERE st.status_id = ANY($1) ORDER BY st.status_id, t.name",
                )
                .bind(&grand_ids)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_cards_f = async {
                sqlx::query(
                    "SELECT status_id, url, title, description, image_path, author_name, author_url, \
                            provider_name, provider_url FROM preview_cards \
                     WHERE status_id = ANY($1) ORDER BY status_id, id",
                )
                .bind(&grand_ids)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_reactions_f = async {
                sqlx::query_as::<_, (i64, String, Option<i64>, i64, Option<bool>)>(
                    "SELECT status_id, emoji, custom_emoji_id, count(*)::bigint, \
                            CASE WHEN $2::bigint IS NULL THEN NULL \
                                 ELSE bool_or(account_id = $2) END AS me \
                     FROM emoji_reactions WHERE status_id = ANY($1) \
                     GROUP BY status_id, emoji, custom_emoji_id \
                     ORDER BY status_id, min(created_at)",
                )
                .bind(&grand_ids)
                .bind(viewer)
                .fetch_all(db)
                .await
                .map_err(to_err)
            };
            let grand_faved_f = async {
                match viewer {
                    Some(v) => sqlx::query_scalar::<_, i64>(
                        "SELECT status_id FROM favourites \
                         WHERE account_id = $1 AND status_id = ANY($2)",
                    )
                    .bind(v)
                    .bind(&grand_ids)
                    .fetch_all(db)
                    .await
                    .map_err(to_err),
                    None => Ok(Vec::new()),
                }
            };
            let grand_marked_f = async {
                match viewer {
                    Some(v) => sqlx::query_scalar::<_, i64>(
                        "SELECT status_id FROM bookmarks \
                         WHERE account_id = $1 AND status_id = ANY($2)",
                    )
                    .bind(v)
                    .bind(&grand_ids)
                    .fetch_all(db)
                    .await
                    .map_err(to_err),
                    None => Ok(Vec::new()),
                }
            };
            let grand_boosted_f = async {
                match viewer {
                    Some(v) => sqlx::query_scalar::<_, i64>(
                        "SELECT reblog_of_id FROM statuses WHERE account_id = $1 \
                         AND reblog_of_id = ANY($2) AND NOT tombstone",
                    )
                    .bind(v)
                    .bind(&grand_ids)
                    .fetch_all(db)
                    .await
                    .map_err(to_err),
                    None => Ok(Vec::new()),
                }
            };
            let (
                grand_statuses,
                g_media,
                g_mentions,
                g_tags,
                g_cards,
                g_reactions,
                g_faved,
                g_marked,
                g_boosted,
            ) = tokio::try_join!(
                grand_statuses_f,
                grand_media_f,
                grand_mentions_f,
                grand_tags_f,
                grand_cards_f,
                grand_reactions_f,
                grand_faved_f,
                grand_marked_f,
                grand_boosted_f
            )?;
            for r in &grand_statuses {
                let s = super::row_to_status(r);
                originals.insert(s.id, s);
            }
            media_rows.extend(g_media);
            mention_rows.extend(g_mentions);
            tag_rows.extend(g_tags);
            card_rows.extend(g_cards);
            reaction_rows.extend(g_reactions);
            faved.extend(g_faved);
            marked.extend(g_marked);
            boosted.extend(g_boosted);
            all_ids.extend(grand_ids);
        }

        // ── Wave 2: inputs that depend on the originals ──
        let mut author_ids: Vec<i64> = rows
            .iter()
            .map(|s| s.account_id)
            .chain(originals.values().map(|s| s.account_id))
            .collect();
        author_ids.sort_unstable();
        author_ids.dedup();

        // The pin check is the exact (author, status) pair — a loose
        // `account_id = ANY` could cross-attribute a pin between two
        // rendered authors, so pairs are filtered client-side.
        let pair_set: HashSet<(i64, i64)> = rows
            .iter()
            .chain(originals.values())
            .map(|s| (s.account_id, s.id))
            .collect();
        let accounts_f = account_mapper::load_many_incl_suspended(db, &author_ids);
        let pins_f = async {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT account_id, status_id FROM status_pins WHERE status_id = ANY($1)",
            )
            .bind(&all_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let mut poll_ids: Vec<i64> = rows
            .iter()
            .chain(originals.values())
            .filter_map(|s| s.poll_id)
            .collect();
        poll_ids.sort_unstable();
        poll_ids.dedup();
        let polls_f = async {
            if poll_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query(
                "SELECT id, expires_at, multiple, hide_totals, created_at \
                 FROM polls WHERE id = ANY($1)",
            )
            .bind(&poll_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let options_f = async {
            if poll_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query_as::<_, (i64, i32, String, i32)>(
                "SELECT poll_id, idx, title, votes_count FROM poll_options \
                 WHERE poll_id = ANY($1) ORDER BY poll_id, idx",
            )
            .bind(&poll_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let votes_f = async {
            if poll_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT poll_id, count(*)::bigint FROM poll_votes \
                 WHERE poll_id = ANY($1) GROUP BY poll_id",
            )
            .bind(&poll_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let voters_f = async {
            if poll_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT poll_id, count(DISTINCT account_id)::bigint FROM poll_votes \
                 WHERE poll_id = ANY($1) GROUP BY poll_id",
            )
            .bind(&poll_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let own_f = async {
            match (viewer, poll_ids.is_empty()) {
                (Some(v), false) => sqlx::query_as::<_, (i64, i32)>(
                    "SELECT poll_id, option_idx FROM poll_votes \
                     WHERE poll_id = ANY($1) AND account_id = $2 \
                     ORDER BY poll_id, option_idx",
                )
                .bind(&poll_ids)
                .bind(v)
                .fetch_all(db)
                .await
                .map_err(to_err),
                _ => Ok(Vec::new()),
            }
        };
        // `posts::can_see` on the originals, batched: the cheap cases
        // (own, public/unlisted, anonymous) resolve in memory; all
        // gated rows share one `status_mentions` lookup (a mention
        // grants audience even on follower-gated posts), and
        // follower-gated authors one `follows` lookup.
        let mut gated_ids = Vec::new();
        let mut follow_targets = Vec::new();
        if viewer.is_some() {
            for o in originals.values() {
                if Some(o.account_id) == viewer
                    || matches!(o.visibility.as_str(), "public" | "unlisted")
                {
                    continue;
                }
                gated_ids.push(o.id);
                if o.visibility != "direct" {
                    follow_targets.push(o.account_id);
                }
            }
        }
        let mention_f = async {
            match (viewer, gated_ids.is_empty()) {
                (Some(v), false) => sqlx::query_scalar::<_, i64>(
                    "SELECT status_id FROM status_mentions \
                     WHERE status_id = ANY($1) AND account_id = $2",
                )
                .bind(&gated_ids)
                .bind(v)
                .fetch_all(db)
                .await
                .map_err(to_err),
                _ => Ok(Vec::new()),
            }
        };
        let follows_f = async {
            match (viewer, follow_targets.is_empty()) {
                (Some(v), false) => sqlx::query_scalar::<_, i64>(
                    "SELECT target_account_id FROM follows \
                     WHERE account_id = $1 AND target_account_id = ANY($2)",
                )
                .bind(v)
                .bind(&follow_targets)
                .fetch_all(db)
                .await
                .map_err(to_err),
                _ => Ok(Vec::new()),
            }
        };
        let (
            author_rows,
            pin_rows,
            poll_rows,
            option_rows,
            vote_rows,
            voter_rows,
            own_rows,
            mention_seen_ids,
            followed,
        ) = tokio::try_join!(
            accounts_f, pins_f, polls_f, options_f, votes_f, voters_f, own_f, mention_f, follows_f
        )?;

        let accounts: HashMap<i64, account_mapper::AccountRow> =
            author_rows.into_iter().map(|a| (a.id, a)).collect();
        let pinned: HashSet<(i64, i64)> = pin_rows
            .into_iter()
            .filter(|p| pair_set.contains(p))
            .collect();

        // `polls::entity` semantics in batch: a referenced poll row
        // that vanished is a hard error, same as the single-status path.
        if poll_rows.len() != poll_ids.len() {
            return Err(crate::Error::NotFound("poll".into()));
        }
        let mut options_by_poll: HashMap<i64, Vec<(i32, String, i32)>> = HashMap::new();
        for (pid, idx, title, votes_count) in option_rows {
            options_by_poll
                .entry(pid)
                .or_default()
                .push((idx, title, votes_count));
        }
        let votes_by_poll: HashMap<i64, i64> = vote_rows.into_iter().collect();
        let voters_by_poll: HashMap<i64, i64> = voter_rows.into_iter().collect();
        let mut own_by_poll: HashMap<i64, Vec<i32>> = HashMap::new();
        for (pid, idx) in own_rows {
            own_by_poll.entry(pid).or_default().push(idx);
        }
        let mut polls = HashMap::with_capacity(poll_rows.len());
        for pr in &poll_rows {
            let pid: i64 = pr.get("id");
            let expires_at: Option<chrono::DateTime<chrono::Utc>> = pr.get("expires_at");
            let expired = expires_at.is_some_and(|e| e <= chrono::Utc::now());
            let hide_totals: bool = pr.get("hide_totals");
            let show_totals = !hide_totals || expired;
            let own_votes = own_by_poll.remove(&pid).unwrap_or_default();
            let options = options_by_poll.remove(&pid).unwrap_or_default();
            polls.insert(
                pid,
                json!({
                    "id": pid.to_string(),
                    "expires_at": expires_at.map(|e| e.to_rfc3339()),
                    "expired": expired,
                    "multiple": pr.get::<bool, _>("multiple"),
                    "votes_count": votes_by_poll.get(&pid).copied().unwrap_or(0),
                    "voters_count": voters_by_poll.get(&pid).copied().unwrap_or(0),
                    "voted": !own_votes.is_empty(),
                    "own_votes": own_votes,
                    "options": options.iter().map(|(_, title, votes_count)| json!({
                        "title": title,
                        "votes_count": if show_totals { Value::from(*votes_count) } else { Value::Null },
                    })).collect::<Vec<_>>(),
                }),
            );
        }

        let mention_seen: HashSet<i64> = mention_seen_ids.into_iter().collect();
        let followed: HashSet<i64> = followed.into_iter().collect();
        let mut visible = HashSet::new();
        for o in originals.values() {
            let ok = if Some(o.account_id) == viewer
                || matches!(o.visibility.as_str(), "public" | "unlisted")
            {
                true
            } else if viewer.is_none() {
                false
            } else if o.visibility == "direct" {
                mention_seen.contains(&o.id)
            } else {
                followed.contains(&o.account_id) || mention_seen.contains(&o.id)
            };
            if ok {
                visible.insert(o.id);
            }
        }

        let mut flags: HashMap<i64, (bool, bool, bool)> = HashMap::new();
        for id in faved {
            flags.entry(id).or_default().0 = true;
        }
        for id in marked {
            flags.entry(id).or_default().1 = true;
        }
        for id in boosted {
            flags.entry(id).or_default().2 = true;
        }

        // ── Wave 3: moved targets + reaction emoji URLs ──
        let mut moved_ids: Vec<i64> = accounts
            .values()
            .filter_map(|a| a.moved_to_account_id)
            .collect();
        moved_ids.sort_unstable();
        moved_ids.dedup();
        let mut custom_emoji_ids: Vec<i64> = reaction_rows
            .iter()
            .filter_map(|(_, _, ce, _, _)| *ce)
            .collect();
        custom_emoji_ids.sort_unstable();
        custom_emoji_ids.dedup();
        let moved_f = async {
            if moved_ids.is_empty() {
                return Ok(Vec::new());
            }
            match account_mapper::load_many_incl_suspended(db, &moved_ids).await {
                Ok(v) => Ok(v),
                // `moved_account_json` degrades to `moved: null` on any
                // failure — keep that contract for the whole page.
                Err(e) => {
                    tracing::warn!(error = %e, "moved-account batch load failed");
                    Ok(Vec::new())
                }
            }
        };
        let ce_f = async {
            if custom_emoji_ids.is_empty() {
                return Ok(Vec::new());
            }
            sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
                "SELECT id, remote_url, image_path FROM custom_emojis WHERE id = ANY($1)",
            )
            .bind(&custom_emoji_ids)
            .fetch_all(db)
            .await
            .map_err(to_err)
        };
        let (moved_rows, ce_rows) = tokio::try_join!(moved_f, ce_f)?;
        let moved_targets: HashMap<i64, account_mapper::AccountRow> =
            moved_rows.into_iter().map(|a| (a.id, a)).collect();
        let ce_urls: HashMap<i64, (Option<String>, Option<String>)> = ce_rows
            .into_iter()
            .map(|(id, remote_url, image_path)| (id, (remote_url, image_path)))
            .collect();

        // ── Wave 4: emoji maps per needed author domain ──
        let mut needed: HashSet<Option<String>> = HashSet::new();
        for s in rows.iter().chain(originals.values()) {
            let Some(author) = accounts.get(&s.account_id) else {
                continue;
            };
            let mut texts: Vec<&str> = vec![
                s.content.as_str(),
                s.spoiler_text.as_deref().unwrap_or_default(),
            ];
            texts.extend(account_mapper::emoji_texts(author));
            if texts
                .iter()
                .any(|t| crate::emoji::render::has_shortcode_candidate(t))
            {
                needed.insert(author.domain.clone());
            }
        }
        for target in moved_targets.values() {
            if account_mapper::emoji_texts(target)
                .iter()
                .any(|t| crate::emoji::render::has_shortcode_candidate(t))
            {
                needed.insert(target.domain.clone());
            }
        }
        let mut emoji_maps = HashMap::with_capacity(needed.len());
        for domain in needed {
            emoji_maps.insert(
                domain.clone(),
                crate::emoji::render::EmojiMap::for_author(
                    db,
                    &ctx.config,
                    &base_url,
                    domain.as_deref(),
                )
                .await?,
            );
        }

        // `moved` entities render after the maps — the target's account
        // gets the same one-level emoji pass `moved_account_json` runs.
        let empty_map = crate::emoji::render::EmojiMap::empty();
        let mut moved = HashMap::new();
        for (aid, account) in &accounts {
            let Some(target) = account
                .moved_to_account_id
                .and_then(|id| moved_targets.get(&id))
            else {
                continue;
            };
            let mut entity = account_mapper::to_account(target, &base_url, &ctx.config);
            let texts = account_mapper::emoji_texts(target);
            let map = if texts
                .iter()
                .any(|t| crate::emoji::render::has_shortcode_candidate(t))
            {
                emoji_maps.get(&target.domain).unwrap_or(&empty_map)
            } else {
                &empty_map
            };
            crate::emoji::render::apply_account_with_map(&mut entity, map);
            moved.insert(*aid, entity);
        }

        // Remaining maps — pure grouping work, no more I/O.
        let mut media: HashMap<i64, Vec<MediaRow>> = HashMap::new();
        for r in &media_rows {
            if let Some(sid) = r.get::<Option<i64>, _>("status_id") {
                media.entry(sid).or_default().push(media_from_row(r));
            }
        }
        let mut mentions: HashMap<i64, Vec<Value>> = HashMap::new();
        for r in &mention_rows {
            let username: String = r.get("username");
            let domain: Option<String> = r.get("domain");
            let url: Option<String> = r.get("url");
            let actor_id: String = r.get("actor_id");
            mentions
                .entry(r.get::<i64, _>("status_id"))
                .or_default()
                .push(json!({
                    "id": r.get::<i64, _>("id").to_string(),
                    "username": username,
                    "acct": match &domain {
                        Some(d) => format!("{username}@{d}"),
                        None => username.clone(),
                    },
                    "url": url.filter(|u| !u.is_empty()).unwrap_or(actor_id),
                }));
        }
        let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
        for (sid, name) in tag_rows {
            tags.entry(sid).or_default().push(name);
        }
        // Rows arrive ordered — first `preview_cards` row per status wins.
        let mut cards: HashMap<i64, Value> = HashMap::new();
        for r in &card_rows {
            let image: Option<String> = r.get("image_path");
            cards
                .entry(r.get::<i64, _>("status_id"))
                .or_insert_with(|| {
                    json!({
                        "url": r.get::<String, _>("url"),
                        "title": r.get::<String, _>("title"),
                        "description": r.get::<String, _>("description"),
                        "type": "link",
                        "author_name": r.get::<String, _>("author_name"),
                        "author_url": r.get::<String, _>("author_url"),
                        "provider_name": r.get::<String, _>("provider_name"),
                        "provider_url": r.get::<String, _>("provider_url"),
                        "image": image.map(|k| media::public_url(&ctx.config, &base_url, &k)),
                        "blurhash": Value::Null,
                    })
                });
        }
        let mut reactions: HashMap<i64, Vec<Value>> = HashMap::new();
        for (sid, emoji, custom_emoji_id, count, me) in reaction_rows {
            let mut entry = json!({
                "name": emoji,
                "count": count,
                "me": me.unwrap_or(false),
            });
            if let Some(ce) = custom_emoji_id {
                if let Some((remote_url, image_path)) = ce_urls.get(&ce) {
                    entry["url"] = remote_url
                        .clone()
                        .filter(|u| !u.is_empty())
                        .or_else(|| {
                            image_path
                                .clone()
                                .map(|k| crate::media::public_url(&ctx.config, &base_url, &k))
                        })
                        .map(Value::String)
                        .unwrap_or(Value::Null);
                }
            }
            reactions.entry(sid).or_default().push(entry);
        }

        Ok(Self {
            accounts,
            moved,
            emoji_maps,
            empty_map,
            originals,
            visible,
            media,
            mentions,
            tags,
            cards,
            polls,
            reactions,
            pinned,
            flags,
        })
    }

    /// The author's domain-scoped emoji map for one status, or the
    /// shared empty map — the same short-circuit
    /// `EmojiMap::for_author_texts` applies per status.
    fn emoji_map<'a>(
        &'a self,
        status: &StatusRow,
        account: &'a account_mapper::AccountRow,
    ) -> &'a crate::emoji::render::EmojiMap {
        let mut texts: Vec<&str> = vec![
            status.content.as_str(),
            status.spoiler_text.as_deref().unwrap_or_default(),
        ];
        texts.extend(account_mapper::emoji_texts(account));
        if texts
            .iter()
            .all(|t| !crate::emoji::render::has_shortcode_candidate(t))
        {
            return &self.empty_map;
        }
        self.emoji_maps
            .get(&account.domain)
            .unwrap_or(&self.empty_map)
    }

    /// The Mastodon-4.4 quote block from batch state — `quoted_status`
    /// renders through [`Self::render`] for entry-level statuses only
    /// (`depth == 0`): the quote inside a reblog's inner status still
    /// renders, but a quote inside a quote does not, as
    /// `interactions::entity::quote_json` produces.
    fn quote_block(
        &self,
        ctx: &PostContext,
        status: &StatusRow,
        viewer: Option<i64>,
        depth: u8,
    ) -> Result<Value> {
        let (id, uri, quoted_status, state) = match (status.quote_of_id, &status.quote_uri) {
            (Some(qid), _) => {
                let quoted_status = match self.originals.get(&qid) {
                    Some(q) if depth == 0 && self.visible.contains(&qid) => {
                        Some(self.render(ctx, q, viewer, depth + 1, false)?)
                    }
                    _ => None,
                };
                (json!(qid.to_string()), None, quoted_status, "accepted")
            }
            (None, Some(uri)) => (Value::Null, Some(uri.clone()), None, "pending"),
            (None, None) => return Ok(Value::Null),
        };
        Ok(json!({
            "quote_id": id,
            "quote": {
                "state": state,
                "quoted_status": quoted_status,
            },
            "yonder_quote_uri": uri,
        }))
    }

    /// Assemble one Status entity purely from batch state. `depth`
    /// counts *quote* nesting — a reblog's inner status renders at the
    /// same depth (a boost doesn't consume quote budget, so its own
    /// quote still renders), while `in_reblog` marks the inner status
    /// so a reblog-of-reblog chain can't recurse.
    fn render(
        &self,
        ctx: &PostContext,
        status: &StatusRow,
        viewer: Option<i64>,
        depth: u8,
        in_reblog: bool,
    ) -> Result<Value> {
        let base_url = ctx.base_url();
        let account = self
            .accounts
            .get(&status.account_id)
            .ok_or_else(|| crate::Error::NotFound("account".into()))?;
        let mut account_json = account_mapper::to_account(account, &base_url, &ctx.config);
        account_json["moved"] = self
            .moved
            .get(&status.account_id)
            .cloned()
            .unwrap_or(Value::Null);
        let emoji_map = self.emoji_map(status, account);
        crate::emoji::render::apply_account_with_map(&mut account_json, emoji_map);
        let media_json: Vec<Value> = self
            .media
            .get(&status.id)
            .into_iter()
            .flatten()
            .map(|m| media_entity(&ctx.config, &base_url, m))
            .collect();
        let poll_json = status
            .poll_id
            .and_then(|id| self.polls.get(&id).cloned())
            .unwrap_or(Value::Null);
        let content = status
            .content_html
            .clone()
            .unwrap_or_else(|| pubby_sdk::content::html_escape(&status.content));
        let (faved, marked, boosted) = self.flags.get(&status.id).copied().unwrap_or_default();
        let flags = json!({
            "reblogged": boosted,
            "favourited": faved,
            "bookmarked": marked,
            "pinned": self.pinned.contains(&(status.account_id, status.id)),
        });
        let quote_block = self.quote_block(ctx, status, viewer, depth)?;
        let emoji_reactions = self.reactions.get(&status.id).cloned().unwrap_or_default();
        let reblog_inner = !in_reblog && depth == 0;
        let (reblog, content, spoiler, media_json, poll_json) =
            match (status.reblog_of_id, reblog_inner) {
                (Some(orig), true) => {
                    let nested = match self.originals.get(&orig) {
                        Some(s) if self.visible.contains(&orig) => {
                            self.render(ctx, s, viewer, depth, true)?
                        }
                        _ => Value::Null,
                    };
                    // Mastodon renders "" for the boost wrapper's own text.
                    (
                        nested,
                        Value::String(String::new()),
                        Value::String(String::new()),
                        Vec::new(),
                        Value::Null,
                    )
                }
                _ => (
                    Value::Null,
                    Value::String(crate::emoji::render::substitute(&content, emoji_map)),
                    Value::String(crate::emoji::render::substitute(
                        &status.spoiler_text.clone().unwrap_or_default(),
                        emoji_map,
                    )),
                    media_json,
                    poll_json,
                ),
            };
        Ok(json!({
            "id": status.id.to_string(),
            "uri": status.uri,
            "url": status.url,
            "account": account_json,
            "content": content,
            "content_type": status.content_type,
            "created_at": status.created_at.to_rfc3339(),
            "edited_at": status.edited_at.map(|e| e.to_rfc3339()),
            "visibility": wire_visibility(&status.visibility),
            "sensitive": status.sensitive,
            "spoiler_text": spoiler,
            "media_attachments": media_json,
            "mentions": self.mentions.get(&status.id).cloned().unwrap_or_default(),
            "tags": self
                .tags
                .get(&status.id)
                .into_iter()
                .flatten()
                .map(|name| json!({
                    "name": name,
                    "url": format!("{}/tags/{name}", base_url.trim_end_matches('/')),
                }))
                .collect::<Vec<_>>(),
            "emojis": emoji_map.entities_in(&format!(
                "{} {}",
                status.content,
                status.spoiler_text.clone().unwrap_or_default()
            )),
            "card": self.cards.get(&status.id).cloned().unwrap_or(Value::Null),
            "poll": poll_json,
            "application": Value::Null,
            "language": status.language,
            "in_reply_to_id": status.in_reply_to_id.map(|i| i.to_string()),
            "in_reply_to_account_id": status.in_reply_to_account_id.map(|i| i.to_string()),
            "reblog": reblog,
            "replies_count": status.replies_count,
            "reblogs_count": status.reblogs_count,
            "favourites_count": status.favourites_count,
            "quotes_count": status.quotes_count,
            "quote_id": quote_block.get("quote_id").cloned().unwrap_or(Value::Null),
            "quote": quote_block.get("quote").cloned().unwrap_or(Value::Null),
            "reblogged": flags["reblogged"],
            "favourited": flags["favourited"],
            "bookmarked": flags["bookmarked"],
            "pinned": flags["pinned"],
            "muted": false,
            "pleroma": {
                "emoji_reactions": emoji_reactions,
            },
            "yonder": {
                "source": status.content,
                "content_type": status.content_type,
                "quote": match (status.quote_of_id, &status.quote_uri) {
                    (Some(id), _) => json!({"id": id.to_string(), "uri": quote_block.get("yonder_quote_uri").cloned().unwrap_or(Value::Null)}),
                    (None, Some(uri)) => json!({"id": Value::Null, "uri": uri}),
                    _ => Value::Null,
                },
            },
        }))
    }
}

/// The Mastodon Status entity. `content` serves the sanitized
/// `content_html` (falling back to escaped `content` for rows that
/// predate the column); the `yonder` block carries the raw `source`,
/// `content_type` and quote data.
pub async fn status_entity(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
) -> Result<Value> {
    Ok(status_entities(ctx, std::slice::from_ref(status), viewer)
        .await?
        .into_iter()
        .next()
        .expect("one entity per status"))
}

/// A page of Status entities sharing one [`StatusBatch`] — order
/// matches `rows`. This is the entry point every list surface
/// (timelines, account statuses, search, context, conversations)
/// should use: a page costs a fixed ~dozen queries in four `try_join!`
/// waves instead of ~8 sequential round-trips per status.
pub async fn status_entities(
    ctx: &PostContext,
    rows: &[StatusRow],
    viewer: Option<i64>,
) -> Result<Vec<Value>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let batch = StatusBatch::load(ctx, rows, viewer).await?;
    rows.iter()
        .map(|s| batch.render(ctx, s, viewer, 0, false))
        .collect()
}

/// `depth` counts quote nesting; reblog traversal doesn't consume it.
/// `pub(crate)` — `interactions::entity::quote_json` recurses through
/// it with `depth + 1`.
pub(crate) async fn status_entity_inner(
    ctx: &PostContext,
    status: &StatusRow,
    viewer: Option<i64>,
    depth: u8,
) -> Result<Value> {
    StatusBatch::load(ctx, std::slice::from_ref(status), viewer)
        .await?
        .render(ctx, status, viewer, depth, false)
}

/// A `scheduled_statuses` row.
#[derive(Debug)]
pub struct ScheduledRow {
    pub id: i64,
    pub account_id: i64,
    pub scheduled_at: chrono::DateTime<chrono::Utc>,
    pub params: Value,
}

/// The Mastodon ScheduledStatus entity — media renders as full
/// MediaAttachment entities from the `scheduled_status_media` join.
pub async fn scheduled_status_entity(ctx: &PostContext, row: &ScheduledRow) -> Result<Value> {
    let base_url = ctx.base_url();
    let media_json: Vec<Value> = media_for_scheduled(ctx, row.id)
        .await?
        .iter()
        .map(|m| media_entity(&ctx.config, &base_url, m))
        .collect();
    Ok(json!({
        "id": row.id.to_string(),
        "scheduled_at": row.scheduled_at.to_rfc3339(),
        "params": row.params,
        "media_attachments": media_json,
    }))
}
