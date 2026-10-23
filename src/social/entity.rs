//! The Mastodon `Relationship` entity — every flag a viewer has
//! toward a target, rendered in one round-trip.
//!
//! `note` is private by construction: it is only ever joined for the
//! *viewer's* `account_notes` row, never for anyone else's.

use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::Result;

/// One relationship row for `viewer_id` → `target_id` — the full
/// Mastodon shape plus the Yonder `subscribing` extension.
/// `blockers_visible` is `admin.blockers_visible`: when false the
/// `blocked_by` field is masked so a blocked user doesn't learn who
/// blocked them.
pub async fn relationship(
    db: &PgPool,
    viewer_id: i64,
    target_id: i64,
    blockers_visible: bool,
) -> Result<Value> {
    let mut out = relationships(db, viewer_id, &[target_id], blockers_visible).await?;
    if out.is_empty() {
        return Err(crate::Error::NotFound("account".into()));
    }
    Ok(out.remove(0))
}

/// Batch render for `GET /api/v1/accounts/relationships?id[]=…` — one
/// query for N targets, in `accounts` order.
pub async fn relationships(
    db: &PgPool,
    viewer_id: i64,
    ids: &[i64],
    blockers_visible: bool,
) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        r#"
        SELECT t.id,
            EXISTS(SELECT 1 FROM follows f
                   WHERE f.account_id = $1 AND f.target_account_id = t.id) AS following,
            COALESCE((SELECT f.show_reblogs FROM follows f
                      WHERE f.account_id = $1 AND f.target_account_id = t.id), false)
                AS showing_reblogs,
            COALESCE((SELECT f.notify FROM follows f
                      WHERE f.account_id = $1 AND f.target_account_id = t.id), false)
                AS notifying,
            EXISTS(SELECT 1 FROM follows f
                   WHERE f.account_id = t.id AND f.target_account_id = $1) AS followed_by,
            EXISTS(SELECT 1 FROM blocks b
                   WHERE b.account_id = $1 AND b.target_account_id = t.id) AS blocking,
            EXISTS(SELECT 1 FROM blocks b
                   WHERE b.account_id = t.id AND b.target_account_id = $1) AS blocked_by,
            EXISTS(SELECT 1 FROM mutes m
                   WHERE m.account_id = $1 AND m.target_account_id = t.id
                     AND (m.expires_at IS NULL OR m.expires_at > now())) AS muting,
            COALESCE((SELECT m.hide_notifications FROM mutes m
                      WHERE m.account_id = $1 AND m.target_account_id = t.id
                        AND (m.expires_at IS NULL OR m.expires_at > now())), false)
                AS muting_notifications,
            EXISTS(SELECT 1 FROM follow_requests r
                   WHERE r.account_id = $1 AND r.target_account_id = t.id) AS requested,
            EXISTS(SELECT 1 FROM follow_requests r
                   WHERE r.account_id = t.id AND r.target_account_id = $1) AS requested_by,
            EXISTS(SELECT 1 FROM account_domain_blocks d
                   WHERE d.account_id = $1 AND d.domain = t.domain) AS domain_blocking,
            EXISTS(SELECT 1 FROM subscriptions s
                   WHERE s.account_id = $1 AND s.target_account_id = t.id) AS subscribing,
            COALESCE((SELECT n.comment FROM account_notes n
                      WHERE n.account_id = $1 AND n.target_account_id = t.id), '') AS note
        FROM accounts t
        WHERE t.id = ANY($2)
        ORDER BY t.id
        "#,
    )
    .bind(viewer_id)
    .bind(ids)
    .fetch_all(db)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let id: i64 = r.get("id");
            json!({
                "id": id.to_string(),
                "following": r.get::<bool, _>("following"),
                "showing_reblogs": r.get::<bool, _>("showing_reblogs"),
                "notifying": r.get::<bool, _>("notifying"),
                "followed_by": r.get::<bool, _>("followed_by"),
                "blocking": r.get::<bool, _>("blocking"),
                "blocked_by": r.get::<bool, _>("blocked_by") && blockers_visible,
                "muting": r.get::<bool, _>("muting"),
                "muting_notifications": r.get::<bool, _>("muting_notifications"),
                "requested": r.get::<bool, _>("requested"),
                "requested_by": r.get::<bool, _>("requested_by"),
                "domain_blocking": r.get::<bool, _>("domain_blocking"),
                "note": r.get::<String, _>("note"),
                "endorsed": false,
                // Yonder extension — subscribe-without-follow.
                "subscribing": r.get::<bool, _>("subscribing"),
            })
        })
        .collect())
}
