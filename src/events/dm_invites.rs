//! Catodon-style event invitations delivered as direct messages.
//!
//! Where Friendica/Hubzilla send `Invite{Event}` activities
//! ([`super::remote::handle_invite`]), Catodon — and Yonder's own
//! DM-invite endpoint — send the invitee a `direct` Note carrying a
//! `yonder:inviteTo`/`catodon:inviteTo` extension property with the
//! event URI. Anyone who can see an event may invite people to it, so
//! — unlike `Invite{Event}` — there is no organizer-authorization
//! check: the invitee's instance materializes an `invited` row and
//! notifies them; RSVP semantics stay with the event's own protocol.
//!
//! Outbound: `events::api::dm_invites` builds one `direct` status per
//! request mentioning every invitee; `StatusParams::event_invite_uri`
//! persists `status_event_invites` inside the create transaction and
//! the AP serializer emits both property names.

use serde_json::Value;

use crate::activitypub::ApCore;
use crate::events::remote;
use crate::posts;
use crate::Result;

/// Extension property names carrying the event URI — our own plus
/// Catodon's, so both dialects interoperate.
const INVITE_KEYS: [&str; 2] = ["yonder:inviteTo", "catodon:inviteTo"];

/// Upper bound on parked DM invites per sender — same rationale as
/// `event_pending_invites`: parking precedes any authorization, so an
/// unbounded table would be a fill vector.
const MAX_PENDING_PER_SENDER: i64 = 256;

/// First invite URI on a note object — accepts a bare string, an
/// `{id|url|href}` object, or an array of either (first hit wins).
pub(crate) fn invite_to_uri(object: &Value) -> Option<String> {
    for key in INVITE_KEYS {
        if let Some(uri) = uri_value(object.get(key)) {
            return Some(uri);
        }
    }
    None
}

fn uri_value(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Array(items)) => items.iter().find_map(|i| uri_value(Some(i))),
        Some(Value::Object(m)) => ["id", "url", "href"].iter().find_map(|k| {
            m.get(*k).and_then(|x| match x {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Object(inner) => inner
                    .get("href")
                    .and_then(|h| h.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                _ => None,
            })
        }),
        _ => None,
    }
}

/// Inbound hook — called for every incoming `Create` after status
/// materialization. Self-guards on the invite property; never fails
/// the activity.
pub async fn on_note(core: &ApCore, activity: &pubby_sdk::Activity, raw: &Value) {
    if let Err(e) = on_note_inner(core, activity, raw).await {
        tracing::warn!(error = %e, "event DM invite handling failed");
    }
}

async fn on_note_inner(core: &ApCore, activity: &pubby_sdk::Activity, raw: &Value) -> Result<()> {
    let object = raw.get("object").cloned().unwrap_or(Value::Null);
    let Some(event_uri) = invite_to_uri(&object) else {
        return Ok(());
    };
    let Some(note_uri) = object
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Ok(());
    };
    // The DM must have materialized and name at least one *local*
    // account — remote-only DMs have nobody to invite here.
    let Some(status) = posts::load_by_uri(&core.db, &note_uri).await? else {
        return Ok(());
    };
    let addressees: Vec<(i64, String)> = sqlx::query_as(
        "SELECT a.id, a.actor_id FROM status_mentions m \
         JOIN accounts a ON a.id = m.account_id \
         WHERE m.status_id = $1 AND a.domain IS NULL",
    )
    .bind(status.id)
    .fetch_all(&core.db)
    .await?;
    if addressees.is_empty() {
        return Ok(());
    }

    match event_status_id(&core.db, &event_uri).await? {
        Some(event_status_id) => {
            let mut tx = core.db.begin().await?;
            let actor_ids: Vec<String> =
                addressees.iter().map(|(_, actor)| actor.clone()).collect();
            // No invite activity exists — `invite_activity_uri` stays
            // NULL so responses fall back to the event URI as object.
            let invited =
                remote::write_invited_rows(&mut tx, event_status_id, &actor_ids, None).await?;
            tx.commit().await?;
            remote::event_invite_received(core, event_status_id, invited).await;
        }
        None => {
            park_dm_invites(core, &event_uri, &note_uri, &activity.actor, &addressees).await?;
            posts::remote::enqueue_status_fetch_deduped(
                &core.redis,
                &core.dispatcher,
                &core.config.jobs,
                &event_uri,
            )
            .await?;
        }
    }
    Ok(())
}

/// `(status_id)` of the materialized `Event` a URI names — matches
/// AP `id` or web `url`, since invite properties may carry either.
async fn event_status_id(db: &sqlx::PgPool, uri: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT id FROM statuses \
         WHERE (uri = $1 OR url = $1) AND object_type = 'Event' AND NOT tombstone \
         LIMIT 1",
    )
    .bind(uri)
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// Park DM invites until their event federates — `addressee` is the
/// local `accounts.id` (always materialized: only local accounts get
/// invited rows). Deduped per (event, note, addressee); capped per
/// sender like `event_pending_invites`.
async fn park_dm_invites(
    core: &ApCore,
    event_uri: &str,
    note_uri: &str,
    actor: &str,
    addressees: &[(i64, String)],
) -> Result<()> {
    let parked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_pending_dm_invites WHERE actor = $1")
            .bind(actor)
            .fetch_one(&core.db)
            .await?;
    if parked + addressees.len() as i64 > MAX_PENDING_PER_SENDER {
        tracing::warn!(
            actor = %actor,
            event_uri = %event_uri,
            parked,
            cap = MAX_PENDING_PER_SENDER,
            "pending DM invite cap reached; invite dropped"
        );
        return Ok(());
    }
    for (account_id, _) in addressees {
        sqlx::query(
            "INSERT INTO event_pending_dm_invites \
                 (event_uri, note_uri, actor, addressee) \
             VALUES ($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(event_uri)
        .bind(note_uri)
        .bind(actor)
        .bind(account_id)
        .execute(&core.db)
        .await?;
    }
    Ok(())
}

/// Apply parked DM invites for a just-materialized event — called
/// post-commit from `posts::remote::upsert` next to
/// [`remote::drain_pending_invites`]. Unlike `Invite{Event}` there is
/// no organizer check: anyone who can see the event may invite.
pub async fn drain_pending_dm_invites(core: &ApCore, status_id: i64) -> Result<()> {
    // Invites may name the event by AP `id` *or* web `url` — drain on
    // both identifiers of the just-materialized status.
    let identifiers: Vec<String> = sqlx::query_scalar(
        "SELECT x FROM statuses s, unnest(ARRAY[s.uri, s.url]) x \
         WHERE s.id = $1 AND x IS NOT NULL",
    )
    .bind(status_id)
    .fetch_all(&core.db)
    .await?;
    let mut tx = core.db.begin().await?;
    let pending: Vec<i64> = sqlx::query_scalar(
        "DELETE FROM event_pending_dm_invites WHERE event_uri = ANY($1) RETURNING addressee",
    )
    .bind(&identifiers)
    .fetch_all(&mut *tx)
    .await?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut invited = Vec::new();
    for account_id in &pending {
        // Resolve the local account's actor_id for `write_invited_rows`.
        let actor_id: Option<String> =
            sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1 AND domain IS NULL")
                .bind(account_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(actor_id) = actor_id else { continue };
        invited.extend(remote::write_invited_rows(&mut tx, status_id, &[actor_id], None).await?);
    }
    tx.commit().await?;
    remote::event_invite_received(core, status_id, invited).await;
    Ok(())
}

/// Retention sweep companion — parked DM invites for events that
/// never federate are dead weight. Called from the remote-activity
/// cleanup alongside `prune_pending_invites`.
pub async fn prune_pending_dm_invites(db: &sqlx::PgPool, retention_days: i64) -> Result<u64> {
    let deleted = sqlx::query(
        "DELETE FROM event_pending_dm_invites \
         WHERE created_at < now() - ($1::int * interval '1 day')",
    )
    .bind(retention_days)
    .execute(db)
    .await?
    .rows_affected();
    if deleted > 0 {
        tracing::info!(deleted, "pending DM event invites pruned");
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invite_to_uri_accepts_string_object_and_array() {
        let uri = "https://events.example/1";
        assert_eq!(
            invite_to_uri(&json!({"yonder:inviteTo": uri})),
            Some(uri.to_string())
        );
        assert_eq!(
            invite_to_uri(&json!({"catodon:inviteTo": {"id": uri}})),
            Some(uri.to_string())
        );
        assert_eq!(
            invite_to_uri(&json!({"catodon:inviteTo": [{"url": {"href": uri}}]})),
            Some(uri.to_string())
        );
        assert_eq!(invite_to_uri(&json!({"content": "hi"})), None);
        assert_eq!(invite_to_uri(&json!({"yonder:inviteTo": ""})), None);
    }
}
