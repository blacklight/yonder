//! Relay administration (epic 22) over the `relays` table created in
//! migration `0005`. Fan-out itself already lives in
//! `activitypub::delivery::enqueue_delivery` — public activities get a
//! `deliveries` ledger row per relay in `state = 'accepted'`, gated on
//! `federation.allow_relays`.
//!
//! Authorization is deliberately manual (resolved decision): creating
//! a relay leaves it `pending`, `authorize` moves it to `accepted`,
//! and no `Follow` activity is sent — `follow_activity_id` stays NULL.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::{http::AppState, Error, Result};

/// One `relays` row: `id, inbox_url, state, created_at, updated_at`.
type Row = (i64, String, String, DateTime<Utc>, DateTime<Utc>);

const COLUMNS: &str = "id, inbox_url, state, created_at, updated_at";

/// Serialize one `relays` row.
fn entity((id, inbox_url, state, created_at, updated_at): &Row) -> Value {
    json!({
        "id": id.to_string(),
        "inbox_url": inbox_url,
        "state": state,
        "created_at": created_at.to_rfc3339(),
        "updated_at": updated_at.to_rfc3339(),
    })
}

/// All relays, oldest first.
pub async fn list(pool: &PgPool) -> Result<Vec<Value>> {
    let rows = sqlx::query_as::<_, Row>(&format!("SELECT {COLUMNS} FROM relays ORDER BY id"))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(entity).collect())
}

/// Add a relay by inbox URL — stored `pending`; nothing is delivered
/// to it until `authorize`. The URL must be absolute https.
pub async fn create(state: &AppState, actor_account_id: i64, inbox_url: &str) -> Result<Value> {
    let parsed = url::Url::parse(inbox_url)
        .map_err(|_| Error::Validation(format!("invalid relay URL {inbox_url:?}")))?;
    if parsed.scheme() != "https" || parsed.host().is_none() {
        return Err(Error::Validation(format!(
            "invalid relay URL {inbox_url:?}"
        )));
    }
    let row = sqlx::query_as::<_, Row>(&format!(
        "INSERT INTO relays (inbox_url) VALUES ($1) RETURNING {COLUMNS}"
    ))
    .bind(inbox_url)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
            Error::Conflict(format!("relay {inbox_url:?} already exists"))
        }
        other => Error::Database(other),
    })?;
    crate::moderation::audit::record(
        &state.db,
        Some(actor_account_id),
        "relay.create",
        "relay",
        Some(row.0),
        json!({ "inbox_url": inbox_url }),
    )
    .await?;
    Ok(entity(&row))
}

/// Move a `pending` relay to `accepted` — manual accept replaces the
/// `Follow`/`Accept` handshake.
pub async fn authorize(state: &AppState, actor_account_id: i64, id: i64) -> Result<Value> {
    let row = sqlx::query_as::<_, Row>(&format!(
        "UPDATE relays SET state = 'accepted', updated_at = now() \
         WHERE id = $1 AND state = 'pending' RETURNING {COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Err(Error::Validation(format!("relay {id} is not pending")));
    };
    crate::moderation::audit::record(
        &state.db,
        Some(actor_account_id),
        "relay.authorize",
        "relay",
        Some(id),
        json!({ "inbox_url": row.1 }),
    )
    .await?;
    Ok(entity(&row))
}

/// Remove a relay row — `pending` or `accepted` alike.
pub async fn remove(state: &AppState, actor_account_id: i64, id: i64) -> Result<()> {
    let inbox_url: Option<String> =
        sqlx::query_scalar("DELETE FROM relays WHERE id = $1 RETURNING inbox_url")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    let Some(inbox_url) = inbox_url else {
        return Err(Error::NotFound("relay".into()));
    };
    crate::moderation::audit::record(
        &state.db,
        Some(actor_account_id),
        "relay.remove",
        "relay",
        Some(id),
        json!({ "inbox_url": inbox_url }),
    )
    .await?;
    Ok(())
}
