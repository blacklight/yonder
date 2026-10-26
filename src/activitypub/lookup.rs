//! Remote-object lookup shared by mention resolution (epic 12) and
//! search (epic 17): WebFinger resolve → bare `accounts` stub →
//! `RemoteActorFetch` enrichment, plus a signed GET for arbitrary
//! remote documents (the search URL-lookup path).

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::jobs::{Dispatcher, JobKind};
use crate::{Error, Result};

use super::{actors::sync_call, delivery::http_client, ApContext};

/// WebFinger-resolve `@username@domain` to an actor URL. Blocking
/// pubby call → `sync_call`; `None` when federation is unavailable
/// or the lookup fails.
///
/// `resolve_actor_url_with_client` fabricates `https://{d}/@{u}` on
/// failure — that guess is not evidence the actor exists, and
/// materializing it would create a stub for a phantom account (the
/// `RemoteActorFetch` enrichment would never succeed). It's filtered
/// here so search and mention resolution both treat it as a miss; a
/// real profile at exactly that path on a WebFinger-less host is the
/// accepted edge case.
pub(crate) async fn resolve_actor_url(
    ap: Option<&ApContext>,
    username: &str,
    domain: &str,
) -> Result<Option<String>> {
    let Some(ap) = ap else {
        return Ok(None);
    };
    let http = http_client(ap);
    let username = username.to_string();
    let domain = domain.to_string();
    let fallback = format!("https://{domain}/@{username}");
    let url = sync_call(move || {
        pubby_sdk::webfinger::resolve_actor_url_with_client(&username, &domain, &http)
    });
    Ok((url != fallback).then_some(url))
}

/// Bare `accounts` upsert for a freshly-resolved remote actor — the
/// `handle_flag` shape (no network I/O, no enrichment job). Returns
/// the account id. Callers wanting the row enriched either enqueue
/// `RemoteActorFetch` themselves ([`upsert_remote_stub`]) or rely on
/// the lazy refresh on the next profile read.
pub(crate) async fn upsert_remote_stub_row(db: &PgPool, actor_url: &str) -> Result<i64> {
    let domain = crate::activitypub::moderation::actor_domain(actor_url)
        .unwrap_or_else(|| "unknown".to_string());
    let username = actor_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(actor_url)
        .trim_start_matches('@')
        .to_string();
    sqlx::query(
        "INSERT INTO accounts (domain, username, actor_id, url, inbox) \
         VALUES ($1,$2,$3,$3,'') ON CONFLICT (actor_id) DO NOTHING",
    )
    .bind(domain)
    .bind(username)
    .bind(actor_url)
    .execute(db)
    .await?;
    sqlx::query_scalar("SELECT id FROM accounts WHERE actor_id = $1")
        .bind(actor_url)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| Error::Internal("account upsert failed".to_string()))
}

/// `upsert_remote_stub_row` + a `RemoteActorFetch` job to enrich it.
pub(crate) async fn upsert_remote_stub(
    db: &PgPool,
    dispatcher: &Dispatcher,
    actor_url: &str,
) -> Result<i64> {
    let id = upsert_remote_stub_row(db, actor_url).await?;
    dispatcher
        .enqueue(
            JobKind::RemoteActorFetch,
            serde_json::json!({"actor_url": actor_url}),
        )
        .await?;
    Ok(id)
}

/// An actor document's collection URL (`followers`, `following`,
/// `outbox`, …) — plain string or `{id: …}` reference.
pub(crate) fn actor_collection_uri(actor_data: &Value, key: &str) -> Option<String> {
    match actor_data.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(m)) => m
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    }
}

/// An AP `totalItems`-style count — integer or numeric string.
pub(crate) fn total_items(doc: &Value) -> Option<i64> {
    doc.get("totalItems")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .map(|n| n.clamp(0, i32::MAX as i64))
}

/// Signed GET of a remote ActivityPub document (actor, Note, …) —
/// the instance actor signs, so `authorized_fetch` servers answer
/// too. `None` when the instance actor lacks key material, the fetch
/// fails, or the body isn't JSON.
pub(crate) async fn fetch_remote_object(ap: &ApContext, url: &str) -> Result<Option<Value>> {
    let actor = super::actors::load_local_actor(&ap.core.db, super::actors::INSTANCE_USERNAME)
        .await?
        .ok_or_else(|| Error::Internal("instance actor missing".to_string()))?;
    let Some(pem) = &actor.private_key_pem else {
        tracing::warn!("instance actor lacks key material; unsigned remote fetch skipped");
        return Ok(None);
    };
    let private_key = pubby_sdk::crypto::keys::load_private_key(pem)
        .map_err(|e| Error::Validation(format!("invalid instance actor key: {e}")))?;
    let key_id = format!("{}#main-key", actor.actor_id);
    let user_agent = ap.core.config.federation.user_agent.clone();
    let http = http_client(ap);
    let url = url.to_string();
    Ok(sync_call(move || {
        let base_headers: HashMap<String, String> = [
            (
                "Accept".to_string(),
                "application/activity+json, application/ld+json".to_string(),
            ),
            ("User-Agent".to_string(), user_agent),
        ]
        .into_iter()
        .collect();
        let opts = pubby_sdk::crypto::signatures::SignOptions {
            headers: base_headers.clone(),
            signed_headers: None,
        };
        let headers = match pubby_sdk::crypto::sign_request(
            &private_key,
            &key_id,
            "GET",
            &url,
            None,
            &opts,
        ) {
            Ok(mut h) => {
                h.extend(base_headers);
                h
            }
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "remote object fetch signing failed");
                return None;
            }
        };
        match http.get(&url, &headers) {
            Ok(resp) if resp.is_success() => resp.json().ok(),
            Ok(resp) => {
                tracing::debug!(status = resp.status, url = %url, "remote object fetch failed");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, url = %url, "remote object fetch error");
                None
            }
        }
    }))
}
