//! Personalized calendar subscription (Section 18) — a secret-token
//! ICS feed at `/users/{username}/calendar/{token}.ics` covering the
//! owner's organized *and* participating events, with the viewer's
//! RSVP rendered as `ATTENDEE;PARTSTAT`.
//!
//! Tokens are 256-bit `OsRng` secrets ([`crate::auth::tokens::
//! random_secret`]); only the SHA-256 hash is stored in
//! `calendar_tokens` — same convention as `oauth_tokens`. Minting
//! revokes any previous token (rotation is implicit); `revoked_at`
//! supports explicit revocation.

use crate::{
    events::{entity, ics},
    Result,
};

/// Participation states that appear in the personal feed — the
/// contract's `invited`/`accepted`/`tentative`/`declined` set.
const FEED_STATES: &[&str] = &["invited", "accepted", "tentative", "declined"];

/// Mint a fresh token for `account_id`, revoking any live one —
/// returns the plaintext token (shown to the user exactly once).
pub async fn mint(db: &sqlx::PgPool, account_id: i64) -> Result<String> {
    let token = crate::auth::tokens::random_secret();
    let hash = crate::http::middleware::auth::token_hash(&token);
    let mut tx = db.begin().await?;
    // Serialize concurrent mints per account — without the row lock
    // two requests could each revoke-then-insert and leave two live
    // tokens.
    sqlx::query("SELECT id FROM accounts WHERE id = $1 FOR UPDATE")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE calendar_tokens SET revoked_at = now() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO calendar_tokens (account_id, token_hash) VALUES ($1, $2)")
        .bind(account_id)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(token)
}

/// Revoke every live token for `account_id`.
pub async fn revoke(db: &sqlx::PgPool, account_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE calendar_tokens SET revoked_at = now() \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(db)
    .await?;
    Ok(())
}

/// `token` → owning `account_id`, or `None` for unknown/revoked.
/// Also stamps `last_used_at`, throttled to one write per hour per
/// token — calendar apps poll aggressively and every fetch would
/// otherwise write.
pub async fn account_for_token(db: &sqlx::PgPool, token: &str) -> Result<Option<i64>> {
    let hash = crate::http::middleware::auth::token_hash(token);
    let account_id = sqlx::query_scalar::<_, i64>(
        "SELECT account_id FROM calendar_tokens \
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;
    if account_id.is_some() {
        sqlx::query(
            "UPDATE calendar_tokens SET last_used_at = now() \
             WHERE token_hash = $1 AND revoked_at IS NULL \
               AND (last_used_at IS NULL OR last_used_at < now() - interval '1 hour')",
        )
        .bind(&hash)
        .execute(db)
        .await?;
    }
    Ok(account_id)
}

/// Status ids in the personal feed: events the account organizes
/// (author or organizer-role participation) plus events it
/// participates in with a feed-worthy state. Remote events included —
/// the token is the authorization, not status visibility. Postponed
/// events (no start) are excluded here — the writer skips them anyway
/// and they must not take cap slots. Windowed to roughly the past year
/// onward and capped at 1000 rows. Upcoming events come first, nearest
/// first, then past events most-recent first, so an overflowing cap
/// drops the *oldest past* events rather than the imminent ones. A
/// heavy calendar therefore doesn't build an unbounded document on
/// every poll.
pub async fn feed_status_ids(db: &sqlx::PgPool, account_id: i64) -> Result<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT ids.status_id FROM ( \
             SELECT p.status_id FROM event_participations p \
             WHERE p.account_id = $1 \
               AND (p.role = 'organizer' OR p.state = ANY($2)) \
             UNION \
             SELECT s.id FROM statuses s \
             WHERE s.account_id = $1 AND s.object_type = 'Event' \
         ) ids \
         JOIN statuses s ON s.id = ids.status_id \
         JOIN events e ON e.status_id = s.id \
         WHERE NOT s.tombstone \
           AND e.start_time IS NOT NULL \
           AND COALESCE(e.end_time, e.start_time) >= now() - interval '1 year' \
         ORDER BY (e.start_time < now()), \
                  CASE WHEN e.start_time >= now() THEN e.start_time END ASC, \
                  CASE WHEN e.start_time < now() THEN e.start_time END DESC, \
                  s.id \
         LIMIT 1000",
    )
    .bind(account_id)
    .bind(FEED_STATES)
    .fetch_all(db)
    .await
    .map_err(Into::into)
}

/// `(status_id → participation state)` for the account across the
/// feed — drives the `PARTSTAT`/`ORGANIZER` lines.
async fn states_for(
    db: &sqlx::PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<std::collections::HashMap<i64, (String, bool)>> {
    if status_ids.is_empty() {
        return Ok(Default::default());
    }
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT status_id, state, role FROM event_participations \
         WHERE account_id = $1 AND status_id = ANY($2)",
    )
    .bind(account_id)
    .bind(status_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, state, role)| (id, (state, role == "organizer")))
        .collect())
}

/// The personalized `VCALENDAR` for `account_id` — every feed event
/// gets an `ORGANIZER` line (owned) or `ATTENDEE;PARTSTAT` line
/// (participating). Events with no start (postponed) are skipped by
/// the writer.
pub async fn calendar_ics(db: &sqlx::PgPool, account_id: i64) -> Result<String> {
    let ids = feed_status_ids(db, account_id).await?;
    let states = states_for(db, account_id, &ids).await?;
    let statuses = crate::posts::load_many(db, &ids).await?;
    let bundles = entity::load_many(db, &ids, None).await?;
    let (username, actor_uri): (String, String) =
        sqlx::query_as("SELECT username, actor_id FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_one(db)
            .await?;
    let cn = username.as_str();
    let uri = actor_uri.as_str();

    let mut out = String::new();
    ics::calendar_open_pub(&format!("Yonder — {username}"), &mut out);
    let mut skipped = 0usize;
    for id in &ids {
        let (Some(status), Some(bundle)) = (statuses.get(id), bundles.get(id)) else {
            continue;
        };
        let party = match states.get(id) {
            Some((_state, true)) => ics::VEventParty {
                organizer: Some((cn, uri)),
                attendee: None,
            },
            Some((state, false)) => match ics::partstat(state) {
                Some(ps) => ics::VEventParty {
                    organizer: None,
                    attendee: Some((ps, cn, uri)),
                },
                None => continue,
            },
            // Authored but no participation row (shouldn't happen —
            // authors get an organizer row) → treat as organizer.
            None if status.account_id == account_id => ics::VEventParty {
                organizer: Some((cn, uri)),
                attendee: None,
            },
            None => continue,
        };
        // `false` = the writer skipped the row (no start time —
        // postponed events have nothing to subscribe to). Counted so
        // the omission is observable in logs rather than silent.
        if !ics::vevent_pub(status, &bundle.row, &bundle.locations, &party, &mut out) {
            skipped += 1;
        }
    }
    if skipped > 0 {
        tracing::info!(account_id, skipped, "calendar feed skipped events");
    }
    ics::calendar_close_pub(&mut out);
    Ok(out)
}
