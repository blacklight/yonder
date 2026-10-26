//! Remote lookup for FQN (`@user@domain`) and URL search queries.
//!
//! DB-side checks always run — a URL or FQN matching a cached row
//! resolves without network I/O. Remote fetches are gated by the
//! caller: `resolve=true` + authenticated + `search.remote_fetch` +
//! federation up (the orchestrator computes `fetch_remote`).
//!
//! Failed lookups are negatively cached in Redis for [`NEG_TTL`] to
//! stop repeat WebFinger/GET hammering.

use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::activitypub::{federation_base_url, lookup, moderation};
use crate::http::AppState;
use crate::posts;
use crate::Result;

/// Negative-lookup cache TTL (10 minutes).
const NEG_TTL: Duration = Duration::from_secs(600);

/// AP actor types an object fetch can materialize.
pub(crate) const ACTOR_TYPES: &[&str] =
    &["Person", "Service", "Application", "Group", "Organization"];

/// AP object types the inbound pipeline materializes as statuses —
/// mirrors `posts::remote::NOTE_TYPES`.
const NOTE_TYPES: &[&str] = &["Note", "Question", "Article", "Page"];

/// What a URL/FQN lookup resolved to.
#[derive(Debug)]
pub enum ResolvedHit {
    Account(i64),
    Status(i64),
}

/// Whether `domain` is instance-suspended (`domain_blocks`,
/// `action='suspend'` — checked live, not from the `ApCore`
/// snapshot).
async fn domain_suspended(db: &PgPool, domain: &str) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM domain_blocks \
         WHERE domain = $1 AND severity = 'suspend')",
    )
    .bind(domain.to_lowercase())
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Whether `key` holds a failed lookup still inside [`NEG_TTL`].
async fn known_miss(state: &AppState, key: &str) -> Result<bool> {
    Ok(state.cache.get(key).await?.is_some())
}

/// Record a failed lookup so repeats within [`NEG_TTL`] skip the network.
async fn remember_miss(state: &AppState, key: &str) -> Result<()> {
    state
        .cache
        .set(key, &json!({"not_found": true}), NEG_TTL)
        .await
}

/// `@user@domain` → `accounts.id` via WebFinger + stub upsert +
/// `RemoteActorFetch` enrichment (shared with mention resolution).
/// Call only when the fetch gates hold — the caller already checked
/// the DB for a cached row.
pub(crate) async fn lookup_fqn(
    state: &AppState,
    username: &str,
    domain: &str,
) -> Result<Option<i64>> {
    if domain_suspended(&state.db, domain).await? {
        return Ok(None);
    }
    let key = format!(
        "search:neglookup:acct:{}@{}",
        username.to_lowercase(),
        domain
    );
    if known_miss(state, &key).await? {
        return Ok(None);
    }
    let Some(ap) = &state.ap else {
        return Ok(None);
    };
    // `resolve_actor_url` already filters pubby's fabricated
    // `https://{d}/@{u}` fallback, so `None` here is a genuine miss.
    match lookup::resolve_actor_url(Some(ap), username, domain).await? {
        Some(actor_url) => Ok(Some(
            lookup::upsert_remote_stub(&state.db, &state.dispatcher, &actor_url).await?,
        )),
        None => {
            remember_miss(state, &key).await?;
            Ok(None)
        }
    }
}

/// `https://…` → the matching entity. Local URLs and cached remote
/// rows resolve from the DB alone; unknown remote URLs take a signed
/// fetch (actor document or status object) behind `fetch_remote`.
pub(crate) async fn lookup_url(
    state: &AppState,
    url: &str,
    fetch_remote: bool,
) -> Result<Option<ResolvedHit>> {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Ok(None),
    };
    if let Some(hit) = lookup_local(state, &parsed).await? {
        return Ok(Some(hit));
    }

    // Cached remote rows: status by uri/url, account by actor id/url.
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM statuses WHERE (uri = $1 OR url = $1) AND NOT tombstone",
    )
    .bind(url)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = id {
        return Ok(Some(ResolvedHit::Status(id)));
    }
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM accounts \
         WHERE (actor_id = $1 OR url = $1) \
           AND suspended_at IS NULL AND deleted_at IS NULL",
    )
    .bind(url)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = id {
        return Ok(Some(ResolvedHit::Account(id)));
    }

    // Remote fetch path — all gates must hold. Our own host never
    // takes it: a miss there is a miss, not a self-request.
    if !fetch_remote || is_local_host(state, &parsed) {
        return Ok(None);
    }
    let Some(ap) = &state.ap else {
        return Ok(None);
    };
    let domain = moderation::actor_domain(url).unwrap_or_default();
    if domain_suspended(&state.db, &domain).await? {
        return Ok(None);
    }
    let key = format!("search:neglookup:url:{url}");
    if known_miss(state, &key).await? {
        return Ok(None);
    }

    let hit = match lookup::fetch_remote_object(ap, url).await? {
        Some(doc) => materialize_remote(state, url, &doc).await?,
        None => None,
    };
    // Fetch failures and fetched-but-unusable documents are both
    // misses; cache them so the same URL isn't re-fetched in the window.
    if hit.is_none() {
        remember_miss(state, &key).await?;
    }
    Ok(hit)
}

/// Materialize a document fetched from `url`, but only when it is
/// trustworthy for that URL (see [`trusted_note`]).
async fn materialize_remote(
    state: &AppState,
    url: &str,
    doc: &Value,
) -> Result<Option<ResolvedHit>> {
    let doc_type = doc.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ACTOR_TYPES.contains(&doc_type) && doc.get("id").and_then(|v| v.as_str()) == Some(url) {
        let id = crate::activitypub::storage::upsert_remote_actor_row(&state.db, url, doc).await?;
        return Ok(id.map(ResolvedHit::Account));
    }
    if NOTE_TYPES.contains(&doc_type) && trusted_note(doc, url) {
        return materialize_status(state, doc).await;
    }
    Ok(None)
}

/// A fetched Note is trusted only when it is served from the host it
/// was requested from and its author lives on that same origin.
/// Without this, a remote host could claim another server's `id` and
/// overwrite that status through the `uri` upsert.
fn trusted_note(doc: &Value, url: &str) -> bool {
    let Some(id) = doc.get("id").and_then(|v| v.as_str()) else {
        return false;
    };
    same_origin(url, id) && attributed_to(doc).is_some_and(|author| same_origin(id, &author))
}

/// `scheme://host:port` equality — the trust boundary for fetched
/// objects. Unparseable URLs are never the same origin.
fn same_origin(a: &str, b: &str) -> bool {
    fn origin(u: &str) -> Option<(String, Option<String>, Option<u16>)> {
        let u = url::Url::parse(u).ok()?;
        Some((
            u.scheme().to_string(),
            u.host_str().map(str::to_lowercase),
            u.port_or_known_default(),
        ))
    }
    origin(a).is_some() && origin(a) == origin(b)
}

/// Wrap a fetched Note in a synthetic `Create` and run it through the
/// inbound materialization path — the instance "learning" a post this
/// way behaves exactly like receiving the Create in the inbox.
async fn materialize_status(state: &AppState, doc: &Value) -> Result<Option<ResolvedHit>> {
    let Some(ap) = &state.ap else {
        return Ok(None);
    };
    let Some(author) = attributed_to(doc) else {
        return Ok(None);
    };
    let object_id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let raw = json!({
        "id": format!("{object_id}#activity"),
        "type": "Create",
        "actor": author,
        "to": doc.get("to").cloned().unwrap_or(Value::Null),
        "cc": doc.get("cc").cloned().unwrap_or(Value::Null),
        "object": doc,
    });
    let activity = pubby_sdk::Activity::from_json(&raw);
    posts::remote::handle(&ap.core, &activity, &raw).await;
    let Some(row) = posts::load_by_uri(&state.db, object_id).await? else {
        return Ok(None);
    };
    Ok(Some(ResolvedHit::Status(row.id)))
}

/// `attributedTo` → actor URL (string, `{id}`, or first array item) —
/// mirrors `posts::remote`'s rule.
fn attributed_to(object: &Value) -> Option<String> {
    match object.get("attributedTo") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("id").and_then(|v| v.as_str()).map(str::to_string),
        Some(Value::Array(a)) => a.first().and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        }),
        _ => None,
    }
}

/// Whether `parsed` points at this instance's federation host.
fn is_local_host(state: &AppState, parsed: &url::Url) -> bool {
    let local_host = url::Url::parse(&federation_base_url(&state.config()))
        .ok()
        .and_then(|u| u.host_str().map(str::to_string));
    parsed.host_str() == local_host.as_deref()
}

/// URL paths Yonder itself serves — actor/profile and status paths,
/// plus the AP route shapes (`activitypub::router`).
async fn lookup_local(state: &AppState, parsed: &url::Url) -> Result<Option<ResolvedHit>> {
    if !is_local_host(state, parsed) {
        return Ok(None);
    }
    let segments: Vec<&str> = parsed
        .path()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let (username, status_id) = match segments.as_slice() {
        // `/@user`, `/@user/{id}` — the public URL shapes
        // (`PostContext::status_urls`, `actor url`).
        [user] if user.starts_with('@') => (Some(&user[1..]), None),
        [user, id] if user.starts_with('@') => match id.parse::<i64>() {
            Ok(n) => (Some(&user[1..]), Some(n)),
            // `/@user/{non-numeric}` is a status-shaped path that can't
            // exist — a definite miss, not an actor lookup.
            Err(_) => (None, None),
        },
        // `/ap/users/{user}`, `/ap/users/{user}/statuses/{id}` — the
        // AP actor/status document paths.
        ["ap", "users", user] => (Some(*user), None),
        ["ap", "users", user, "statuses", id] => (Some(*user), id.parse::<i64>().ok()),
        _ => (None, None),
    };
    if let Some(id) = status_id {
        // The numeric id is authoritative — the username part of a
        // status URL is decorative, like Mastodon's.
        let found: Option<i64> =
            sqlx::query_scalar("SELECT id FROM statuses WHERE id = $1 AND local")
                .bind(id)
                .fetch_optional(&state.db)
                .await?;
        return Ok(found.map(ResolvedHit::Status));
    }
    if let Some(user) = username {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM accounts \
             WHERE lower(username) = lower($1) AND domain IS NULL \
               AND suspended_at IS NULL AND deleted_at IS NULL",
        )
        .bind(user)
        .fetch_optional(&state.db)
        .await?;
        return Ok(found.map(ResolvedHit::Account));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn same_origin_compares_scheme_host_port() {
        assert!(same_origin("https://a.example/x", "https://a.example/y"));
        assert!(same_origin(
            "https://a.example:443/x",
            "https://a.example/y"
        ));
        assert!(!same_origin("https://a.example/x", "https://b.example/x"));
        assert!(!same_origin("https://a.example/x", "http://a.example/x"));
        assert!(!same_origin("not a url", "https://a.example/x"));
        assert!(!same_origin("https://a.example/x", "not a url"));
    }

    #[test]
    fn trusted_note_requires_object_and_author_on_fetch_origin() {
        let url = "https://a.example/notes/1";
        let ok = json!({"id": url, "attributedTo": "https://a.example/users/eve"});
        assert!(trusted_note(&ok, url));
        // A host claiming another server's id — the classic spoof.
        let spoofed = json!({
            "id": "https://victim.example/notes/9",
            "attributedTo": "https://victim.example/users/eve",
        });
        assert!(!trusted_note(&spoofed, url));
        // Object on the right origin but authored elsewhere.
        let foreign_author = json!({"id": url, "attributedTo": "https://b.example/users/eve"});
        assert!(!trusted_note(&foreign_author, url));
        // No id at all.
        assert!(!trusted_note(
            &json!({"attributedTo": "https://a.example/users/eve"}),
            url
        ));
    }
}
