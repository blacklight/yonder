//! Local actor provisioning, key management and the
//! [`MultiActorHandler`]'s [`ActorResolver`].
//!
//! Every local `accounts` row (`domain IS NULL`) maps to one
//! [`ActivityPubHandler`]: the resolver builds it from the row +
//! [`ActorConfig`] + [`HandlerOptions`] and caches it in a small LRU —
//! RSA-private-key parsing is the expensive step, and actor documents
//! are requested on nearly every federation interaction.
//!
//! `provision_local_actor` is also the **public hook for epic 07**:
//! registration calls it after creating the account row; the startup
//! path uses it for the singleton instance actor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pubby_sdk::crypto::keys::{
    export_private_key_pem, export_public_key_pem, generate_rsa_keypair,
};
use pubby_sdk::handlers::HandlerOptions;
use pubby_sdk::{
    ActivityPubError, ActivityPubHandler, ActorConfig, ActorResolver, DocumentCache,
    MultiActorHandler, NodeInfoStats,
};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Row};

use crate::config::Config;
use crate::Result;

/// Reserved username of the instance actor (singleton local account
/// that signs instance-level fetches and fronts relays). Must be
/// excluded from registration — noted for epic 07's username blocklist.
pub const INSTANCE_USERNAME: &str = "yonder.instance";

/// Actor URL path template — the `{username}` segment is both pubby's
/// placeholder and axum's path param syntax.
pub const ACTOR_PATH_TEMPLATE: &str = "/ap/users/{username}";

/// Handlers kept per process; on overflow the whole map resets
/// (rebuilds are cheap — one RSA parse).
const HANDLER_CACHE_CAP: usize = 1000;

/// Per-username handler cache shared by the resolver and any future
/// invalidation call sites (actor `Update`, key rotation).
pub type HandlerCache = Arc<Mutex<HashMap<String, Arc<ActivityPubHandler>>>>;

/// Drop all cached per-user handlers (call on actor updates or key
/// rotation — epics 09/11 wire this up).
pub fn invalidate_handlers(cache: &HandlerCache, username: Option<&str>) {
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    match username {
        Some(u) => {
            map.remove(&u.to_lowercase());
        }
        None => map.clear(),
    }
}

/// The actor path for `username` under [`ACTOR_PATH_TEMPLATE`].
pub fn actor_path(username: &str) -> String {
    ACTOR_PATH_TEMPLATE.replace("{username}", username)
}

/// The full actor id for `username`.
pub fn actor_id(base_url: &str, username: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), actor_path(username))
}

/// Local account row fields the handler build needs.
#[derive(Debug)]
pub struct LocalActor {
    /// `accounts.id`.
    pub id: i64,
    /// Lowercase login name.
    pub username: String,
    /// `{base}/ap/users/{username}`.
    pub actor_id: String,
    /// Profile URL (`{base}/@{username}`).
    pub url: String,
    /// Display name (falls back to username).
    pub display_name: Option<String>,
    /// Profile bio (HTML).
    pub bio: Option<String>,
    /// `accounts.locked` → `manually_approves_followers`.
    pub locked: bool,
    /// `accounts.bot` → `Service` actor type.
    pub bot: bool,
    /// Avatar storage key or remote URL for `icon` (remote wins).
    pub avatar_path: Option<String>,
    /// Remote avatar URL, if any.
    pub avatar_remote_url: Option<String>,
    /// Banner storage key or remote URL — `image` on the actor document
    /// (injected at publish; pubby `ActorConfig` has no banner field).
    pub banner_path: Option<String>,
    /// Remote banner URL, if any.
    pub banner_remote_url: Option<String>,
    /// `profile_fields` rows → `PropertyValue` attachments.
    pub profile_fields: Value,
    /// PEM private key (migration 0009); `None` when unprovisioned.
    pub private_key_pem: Option<String>,
    /// `moved_to_account_id` → target `accounts.actor_id`.
    pub moved_to: Option<String>,
    /// `account_aliases` URIs → `alsoKnownAs`.
    pub also_known_as: Vec<String>,
}

/// Load a local account row by username (case-insensitive) including
/// its `account_aliases` — `None` for remote or unknown users.
pub async fn load_local_actor(pool: &PgPool, username: &str) -> Result<Option<LocalActor>> {
    let row = sqlx::query(
        r#"
        SELECT a.id, a.username, a.actor_id, a.url, a.display_name, a.bio,
               a.locked, a.bot, a.avatar_path, a.avatar_remote_url,
               a.banner_path, a.banner_remote_url,
               a.profile_fields, a.private_key_pem,
               t.actor_id AS moved_to
        FROM accounts a
        LEFT JOIN accounts t ON t.id = a.moved_to_account_id
        WHERE lower(a.username) = lower($1) AND a.domain IS NULL AND a.deleted_at IS NULL
        "#,
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    let aliases: Vec<String> =
        sqlx::query_scalar("SELECT uri FROM account_aliases WHERE account_id = $1 ORDER BY id")
            .bind(r.get::<i64, _>("id"))
            .fetch_all(pool)
            .await?;
    Ok(Some(LocalActor {
        id: r.get("id"),
        username: r.get("username"),
        actor_id: r.get("actor_id"),
        url: r.get::<Option<String>, _>("url").unwrap_or_default(),
        display_name: r.get("display_name"),
        bio: r.get("bio"),
        locked: r.get("locked"),
        bot: r.get("bot"),
        avatar_path: r.get("avatar_path"),
        avatar_remote_url: r.get("avatar_remote_url"),
        banner_path: r.get("banner_path"),
        banner_remote_url: r.get("banner_remote_url"),
        profile_fields: r.get("profile_fields"),
        private_key_pem: r.get("private_key_pem"),
        moved_to: r.get("moved_to"),
        also_known_as: aliases,
    }))
}

/// Provision AP identity for an existing local `accounts` row:
/// generates the RSA keypair and fills `actor_id`/`url`/inbox/outbox/
/// keys. Returns `(actor_id, updated)` — `updated=false` means the row
/// was already provisioned and the **stored** `actor_id` is returned.
///
/// `actor_id` is the federation identity, so provisioning is not
/// retroactive: changing `federation.base_url` leaves existing rows on
/// their original URLs (rotate/re-home explicitly instead of silently
/// rekeying).
///
/// Call for every new local account (epic 07 registration hook) and by
/// `ensure_instance_actor` at startup.
pub async fn provision_local_actor(
    pool: &PgPool,
    base_url: &str,
    account_db_id: i64,
    username: &str,
) -> Result<(String, bool)> {
    let mut conn = pool.acquire().await?;
    provision_local_conn(&mut conn, base_url, account_db_id, username).await
}

/// [`provision_local_actor`] on an explicit connection — lets callers
/// bundle provisioning into a transaction (`ensure_instance_actor`).
pub async fn provision_local_conn(
    conn: &mut PgConnection,
    base_url: &str,
    account_db_id: i64,
    username: &str,
) -> Result<(String, bool)> {
    let path = actor_path(username);
    let actor_id = format!("{}{}", base_url.trim_end_matches('/'), path);
    let inbox = format!("{actor_id}/inbox");
    let shared_inbox = format!("{}/ap/inbox", base_url.trim_end_matches('/'));
    let outbox = format!("{actor_id}/outbox");
    let profile_url = format!("{}/@{username}", base_url.trim_end_matches('/'));

    let (private_key, public_key) = generate_rsa_keypair(2048)
        .map_err(|e| crate::Error::Internal(format!("keygen failed: {e}")))?;
    let public_pem = export_public_key_pem(&public_key)
        .map_err(|e| crate::Error::Internal(format!("public key export failed: {e}")))?;
    let private_pem = export_private_key_pem(&private_key)
        .map_err(|e| crate::Error::Internal(format!("private key export failed: {e}")))?;

    let updated = sqlx::query(
        r#"
        UPDATE accounts SET
            actor_id = $2, url = $3, inbox = $4, shared_inbox = $5,
            outbox_url = $6, public_key = $7, private_key_pem = $8,
            updated_at = now()
        WHERE id = $1 AND private_key_pem IS NULL
        "#,
    )
    .bind(account_db_id)
    .bind(&actor_id)
    .bind(&profile_url)
    .bind(&inbox)
    .bind(&shared_inbox)
    .bind(&outbox)
    .bind(&public_pem)
    .bind(&private_pem)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;
    if updated {
        return Ok((actor_id, true));
    }
    // Already provisioned — report the stored identity, not the one
    // this call would have written.
    let stored: Option<String> = sqlx::query_scalar("SELECT actor_id FROM accounts WHERE id = $1")
        .bind(account_db_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok((stored.unwrap_or(actor_id), false))
}

/// Create or fetch the singleton instance actor account and make sure
/// it carries a key — in one transaction, so a failure mid-way can't
/// leave a keyless row that `handler_for` would then reject at boot.
/// Returns its [`LocalActor`].
pub async fn ensure_instance_actor(pool: &PgPool, base_url: &str) -> Result<LocalActor> {
    let mut tx = pool.begin().await?;
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1 AND domain IS NULL")
            .bind(INSTANCE_USERNAME)
            .fetch_optional(&mut *tx)
            .await?;
    let id = match existing {
        Some(id) => id,
        None => {
            let actor_id = actor_id(base_url, INSTANCE_USERNAME);
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO accounts (domain, username, actor_id, bot) \
                 VALUES (NULL, $1, $2, true) RETURNING id",
            )
            .bind(INSTANCE_USERNAME)
            .bind(&actor_id)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    let needs_key: bool =
        sqlx::query_scalar("SELECT private_key_pem IS NULL FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if needs_key {
        provision_local_conn(&mut tx, base_url, id, INSTANCE_USERNAME).await?;
    }
    tx.commit().await?;
    load_local_actor(pool, INSTANCE_USERNAME)
        .await?
        .ok_or_else(|| crate::Error::Internal("instance actor missing after ensure".to_string()))
}

/// Build the [`ActorConfig`] for a local actor. Per-actor endpoint paths
/// (the 0.6.0 `*_path` fields) put inbox/outbox/followers/following
/// under the actor instead of the instance-wide `/ap/*` defaults.
fn actor_config(base_url: &str, actor: &LocalActor, actor_path: &str) -> ActorConfig {
    let username = &actor.username;
    let mut cfg = ActorConfig::new(base_url);
    cfg.username = username.clone();
    cfg.name = actor
        .display_name
        .clone()
        .or_else(|| Some(username.clone()));
    // Local bios are stored raw — render (escape + linkify) here so a
    // `<` in the bio can't leak into the AP `summary`.
    cfg.summary = actor
        .bio
        .as_deref()
        .map(pubby_sdk::render_bio_html)
        .unwrap_or_default();
    cfg.icon_url = actor
        .avatar_remote_url
        .clone()
        .or_else(|| {
            actor
                .avatar_path
                .as_ref()
                .map(|k| crate::media::public_url(base_url, k))
        })
        .unwrap_or_default();
    cfg.actor_path = actor_path.to_string();
    cfg.actor_type = if actor.bot {
        "Service".to_string()
    } else if username == INSTANCE_USERNAME {
        "Application".to_string()
    } else {
        "Person".to_string()
    };
    cfg.manually_approves_followers = actor.locked;
    cfg.url = Some(actor.url.clone());
    cfg.liked = Some(format!("{}/liked", actor.actor_id));
    cfg.featured = Some(format!("{}/collections/featured", actor.actor_id));
    cfg.moved_to = actor.moved_to.clone();
    cfg.also_known_as = actor.also_known_as.clone();
    cfg.inbox_path = Some(format!("{actor_path}/inbox"));
    cfg.outbox_path = Some(format!("{actor_path}/outbox"));
    cfg.followers_path = Some(format!("{actor_path}/followers"));
    cfg.following_path = Some(format!("{actor_path}/following"));
    cfg.attachment = actor
        .profile_fields
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?;
                    let value = f.get("value")?.as_str()?;
                    Some(json!({
                        "type": "PropertyValue",
                        "name": name,
                        "value": value,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    cfg.normalize();
    cfg
}

/// Shared `HandlerOptions` for every local handler. `stats` is only set
/// on the instance handler (per-user nodeinfo would leak counts).
pub fn handler_options(
    config: &Config,
    doc_cache: &Arc<DocumentCache>,
    blocked: Vec<String>,
    stats: Option<pubby_sdk::handlers::NodeInfoStatsFn>,
) -> HandlerOptions {
    HandlerOptions {
        local_base_urls: vec![super::federation_base_url(config)],
        blocked_instances: Some(blocked),
        strict_attribution: true,
        // Deliberate (confirmed): auto-authorize every QuoteRequest —
        // Mastodon's default; per-user consent is a later epic.
        auto_approve_quotes: true,
        document_cache: Some(doc_cache.clone()),
        async_delivery: false,
        http_timeout: config.federation.request_timeout_seconds as f64,
        user_agent: Some(config.federation.user_agent.clone()),
        allow_private_targets: config.federation.allow_private_targets,
        software_name: "Yonder".to_string(),
        software_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        nodeinfo_stats: stats,
        ..HandlerOptions::default()
    }
}

/// Build an [`ActivityPubHandler`] for a local actor — `None` when the
/// account has no key material yet (unprovisioned rows).
pub fn handler_for(
    storage: Arc<dyn pubby_sdk::ActivityPubStorage>,
    base_url: &str,
    actor: &LocalActor,
    actor_path: &str,
    options: HandlerOptions,
) -> Result<Option<ActivityPubHandler>> {
    let Some(pem) = &actor.private_key_pem else {
        tracing::warn!(username = %actor.username, "local actor has no key material");
        return Ok(None);
    };
    let private_key = pubby_sdk::crypto::keys::load_private_key(pem)
        .map_err(|e| crate::Error::Validation(format!("invalid actor key: {e}")))?;
    let handler = ActivityPubHandler::new(
        storage,
        actor_config(base_url, actor, actor_path),
        private_key,
        options,
    )
    .map_err(|e| crate::Error::Internal(format!("handler build failed: {e}")))?;
    Ok(Some(handler))
}

/// Build the [`ActorResolver`] for [`MultiActorHandler`]: lowercases the
/// lookup, hits the handler LRU, and falls through to
/// `load_local_actor` + `handler_for` on a miss.
pub fn make_resolver(core: Arc<super::ApCore>, cache: HandlerCache) -> ActorResolver {
    Arc::new(move |username: &str| {
        let username = username.to_lowercase();
        if let Some(h) = cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&username)
            .cloned()
        {
            return Ok(Some(h));
        }
        let actor = sync_block(load_local_actor(&core.db, &username))
            .map_err(|e| ActivityPubError::General(e.to_string()))?;
        let Some(actor) = actor else {
            return Ok(None);
        };
        let mut options = super::per_user_handler_options(&core);
        super::bind_follow_response(&mut options, &core, &actor.actor_id);
        let handler = handler_for(
            core.storage_trait(),
            &core.base_url,
            &actor,
            &actor_path(&username),
            options,
        )
        .map_err(|e| ActivityPubError::General(e.to_string()))?;
        let Some(handler) = handler else {
            return Ok(None);
        };
        let handler = Arc::new(handler);
        let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= HANDLER_CACHE_CAP {
            map.clear();
        }
        map.insert(username, handler.clone());
        Ok(Some(handler))
    })
}

/// Build the [`MultiActorHandler`] from the instance handler and the
/// resolver; `webfinger_domain` stays `instance.domain` even when
/// `federation.base_url` carries a dev scheme (Mastodon
/// LOCAL_DOMAIN/WEB_DOMAIN split).
pub fn build_multi(
    instance: ActivityPubHandler,
    resolver: ActorResolver,
    webfinger_domain: &str,
) -> MultiActorHandler {
    let mut multi = MultiActorHandler::new(Arc::new(instance), resolver, ACTOR_PATH_TEMPLATE);
    multi.webfinger_domain = webfinger_domain.to_string();
    multi
}

/// The `NodeInfoStatsFn` bound on the instance handler.
pub fn nodeinfo_stats_fn(core: Arc<super::ApCore>) -> pubby_sdk::handlers::NodeInfoStatsFn {
    Arc::new(move || {
        let (users, posts) = sync_block(async {
            let users = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM accounts \
                 WHERE domain IS NULL AND username <> $1",
            )
            .bind(INSTANCE_USERNAME)
            .fetch_one(&core.db)
            .await
            .unwrap_or(0);
            // Public-facing count: public + unlisted local statuses
            // only — replies/direct stay out of NodeInfo.
            let posts = sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM statuses s \
                 JOIN accounts a ON a.id = s.account_id \
                 WHERE a.domain IS NULL AND s.visibility IN ('public', 'unlisted')",
            )
            .fetch_one(&core.db)
            .await
            .unwrap_or(0);
            (users.max(0) as u64, posts.max(0) as u64)
        });
        NodeInfoStats {
            users,
            posts,
            open_registrations: !matches!(
                core.config.auth.registration_mode,
                crate::config::RegistrationMode::Closed
            ),
        }
    })
}

/// Run an async query inside a sync pubby callback. Same bridge as the
/// storage adapter — multi-thread runtime required.
pub(crate) fn sync_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Run a synchronous pubby call (`dispatch_multi`,
/// `process_inbox_activity`, `publish_activity`, `fetch_actor`) on the
/// current worker while freeing the runtime slot — the call itself may
/// block on network/storage. Multi-thread runtime required.
pub(crate) fn sync_call<T>(f: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(f)
}
