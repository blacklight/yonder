//! ActivityPub federation via `pubby-sdk` — epic 06.
//!
//! [`ApContext`] is the shared state: a [`MultiActorHandler`] (instance
//! actor + per-username [`ActorResolver`]), the Postgres storage
//! adapter, the Redis-backed document cache and the job dispatcher.
//!
//! Routing: POST inboxes stay custom (validate → dedupe → enqueue → 202,
//! see [`inbox`]) so processing is durable; every GET goes through
//! pubby's `server::dispatch_multi` behind a thin axum adapter
//! ([`adapter`]) — actor documents, WebFinger, NodeInfo, host-meta,
//! outbox/followers/following and quote authorizations come for free.
//!
//! Requires the multi-thread tokio runtime: `ActivityPubStorage` and
//! the pubby callbacks are synchronous and bridge via `block_in_place`.

pub mod actors;
pub mod adapter;
pub mod delivery;
pub mod follows;
pub mod inbox;
pub mod incoming;
pub mod moderation;
pub mod posts;
pub mod storage;

use std::sync::Arc;

use axum::Router;
use pubby_sdk::handlers::HandlerOptions;
use pubby_sdk::{ActivityPubHandler, DocumentCache, MultiActorHandler};
use sqlx::PgPool;

use crate::cache::Redis;
use crate::config::Config;
use crate::http::AppState;
use crate::jobs::Dispatcher;
use crate::Result;

use actors::HandlerCache;

/// Internals shared by the resolver and the pubby callbacks — kept
/// separate from [`ApContext`] so closures can capture an `Arc` without
/// creating a reference cycle through `multi`.
pub struct ApCore {
    /// PostgreSQL pool.
    pub db: PgPool,
    /// The storage adapter every handler shares.
    pub storage: Arc<storage::PgActivityPubStorage>,
    /// Redis-backed document cache for dereference endpoints.
    pub doc_cache: Arc<DocumentCache>,
    /// Effective configuration.
    pub config: Arc<Config>,
    /// Job enqueue access for callbacks (deliveries, side-effects).
    pub dispatcher: Dispatcher,
    /// Public AP base URL (`federation.base_url` or
    /// `https://{instance.domain}`).
    pub base_url: String,
    /// Snapshot of suspended domains (config ∪ `domain_blocks`,
    /// `action='suspend'`). Rebuilt per process — moderation changes
    /// take effect on restart; epic 21 can re-resolve handlers to
    /// apply earlier.
    pub blocked: Vec<String>,
}

impl ApCore {
    /// The storage as the pubby trait object handlers expect.
    pub fn storage_trait(&self) -> Arc<dyn pubby_sdk::ActivityPubStorage> {
        self.storage.clone()
    }
}

/// Federation shared state — `AppState.ap` is `Some` when
/// `federation.enabled`.
#[derive(Clone)]
pub struct ApContext {
    /// Instance actor + per-username resolver.
    pub multi: Arc<MultiActorHandler>,
    /// Shared internals (db, storage, cache, config, dispatcher).
    pub core: Arc<ApCore>,
    /// Per-username handler LRU.
    pub handlers: HandlerCache,
}

/// `HandlerOptions` for a per-user handler: shared options plus the
/// `on_activity`/`on_follow_response`/`on_interaction_received`
/// callbacks and the queued `DeliverFn` seam.
pub fn per_user_handler_options(core: &Arc<ApCore>) -> HandlerOptions {
    let mut options =
        actors::handler_options(&core.config, &core.doc_cache, core.blocked.clone(), None);
    options.deliver = Some(delivery::deliver_fn(core));
    options.on_activity = Some(incoming::on_activity_fn(core));
    options.on_interaction_received = Some(incoming::on_interaction_fn(core));
    options
}

/// Bind the per-actor `on_follow_response` — the callback fires with
/// only the remote target URL, so the local follower is captured here
/// (it is always the handler's own actor id).
pub fn bind_follow_response(
    options: &mut HandlerOptions,
    core: &Arc<ApCore>,
    local_actor_id: &str,
) {
    options.on_follow_response = Some(follows::on_follow_response_fn(core, local_actor_id));
}

/// `HandlerOptions` for the instance actor — same callbacks plus
/// `nodeinfo_stats` (the report lives on the instance actor's handler).
fn instance_handler_options(core: &Arc<ApCore>) -> HandlerOptions {
    let mut options = per_user_handler_options(core);
    options.nodeinfo_stats = Some(actors::nodeinfo_stats_fn(core.clone()));
    options.follow_policy = Some(Arc::new(|_: &str, _: &str| {
        Some(pubby_sdk::FollowPolicy::Accept)
    }));
    options
}

/// Build [`ApContext`] at startup: storage, document cache, instance
/// actor, resolver and `MultiActorHandler`.
pub async fn build(
    config: &Config,
    pool: &PgPool,
    redis: &Redis,
    dispatcher: &Dispatcher,
) -> Result<ApContext> {
    let base_url = federation_base_url(config);
    let storage = Arc::new(storage::PgActivityPubStorage::new(pool.clone()));
    let doc_cache = Arc::new(DocumentCache::new(
        Some(Arc::new(pubby_sdk::RedisDocumentStore::with_prefix(
            redis.client().clone(),
            format!("{}:ap:", redis.prefix()),
        ))),
        pubby_sdk::cache::DEFAULT_MAX_ENTRIES,
        60.0,
        pubby_sdk::cache::DEFAULT_MISS_TTL,
        pubby_sdk::cache::DEFAULT_STALE_FACTOR,
        Some(30.0),
    ));
    let core = Arc::new(ApCore {
        db: pool.clone(),
        storage,
        doc_cache,
        config: Arc::new(config.clone()),
        dispatcher: dispatcher.clone(),
        base_url: base_url.clone(),
        blocked: moderation::blocked_domains(pool, config).await?,
    });

    let instance_actor = actors::ensure_instance_actor(pool, &base_url).await?;
    let mut instance_options = instance_handler_options(&core);
    bind_follow_response(&mut instance_options, &core, &instance_actor.actor_id);
    let instance = actors::handler_for(
        core.storage_trait(),
        &base_url,
        &instance_actor,
        "/ap/actor",
        instance_options,
    )?
    .ok_or_else(|| crate::Error::Internal("instance actor provisioning failed".to_string()))?;

    let handlers = actors::HandlerCache::default();
    let resolver = actors::make_resolver(core.clone(), handlers.clone());
    let multi = Arc::new(actors::build_multi(
        instance,
        resolver,
        &config.instance.domain,
    ));

    Ok(ApContext {
        multi,
        core,
        handlers,
    })
}

/// The effective public base URL: `federation.base_url` when set,
/// otherwise `https://{instance.domain}` (trimmed).
pub fn federation_base_url(config: &Config) -> String {
    config
        .federation
        .base_url
        .clone()
        .unwrap_or_else(|| format!("https://{}", config.instance.domain))
        .trim_end_matches('/')
        .to_string()
}

/// Register the real federation job handlers, replacing the stubs for
/// `FederationDelivery`/`FederationIncoming`/`RemoteActorFetch`.
///
/// `RemoteActorFetch` gets its real handler (refresh `actor_cache` +
/// `accounts`) via [`incoming`]; the other two are the inbound and
/// outbound pipelines.
pub fn register_handlers(registry: &mut crate::jobs::Registry, ctx: &ApContext) {
    registry.register(
        crate::jobs::JobKind::FederationIncoming,
        incoming::FederationIncomingJob::new(ctx.clone()),
    );
    registry.register(
        crate::jobs::JobKind::FederationDelivery,
        delivery::FederationDeliveryJob::new(ctx.clone()),
    );
    registry.register(
        crate::jobs::JobKind::RemoteActorFetch,
        incoming::RemoteActorFetchJob::new(ctx.clone()),
    );
    registry.register(
        crate::jobs::JobKind::FederationDeliverySweep,
        delivery::FederationDeliverySweepJob,
    );
}

/// Build the federation router. Merged in [`crate::http::build_router`]
/// when `federation.enabled`; the AP surface lives outside `/api` and is
/// excluded from the OpenAPI document.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    use axum::routing::{get, post};

    let serve = |s: &Arc<AppState>| {
        let s = s.clone();
        move |req| adapter::serve(s.clone(), req)
    };
    Router::new()
        // POST inboxes: custom durable pipeline (not dispatch_multi —
        // pubby would process inline on the runtime thread).
        .route("/ap/inbox", post(inbox::post_shared))
        .route("/ap/users/{username}/inbox", post(inbox::post_per_user))
        .route_layer(crate::http::middleware::ratelimit::layer(
            state,
            "federation_inbox",
        ))
        // Status object documents — visibility-scoped, custom handler
        // (registered before the dispatch_multi catch-all).
        .route(
            "/ap/users/{username}/statuses/{id}",
            get(posts::serve_object),
        )
        // Every GET under /ap/* fans into dispatch_multi.
        .route("/ap/{*rest}", get(serve(state)))
        // Well-known + nodeinfo endpoints.
        .route("/.well-known/webfinger", get(serve(state)))
        .route("/.well-known/nodeinfo", get(serve(state)))
        .route("/.well-known/host-meta", get(serve(state)))
        .route("/.well-known/host-meta.json", get(serve(state)))
        .route("/nodeinfo/{version}", get(serve(state)))
}

/// Convenience for tests and future call sites: the resolved handler
/// for `username` (`None` for unknown users).
pub fn handler_for_username(
    ctx: &ApContext,
    username: &str,
) -> Result<Option<Arc<ActivityPubHandler>>> {
    ctx.multi
        .for_username(username)
        .map_err(|e| crate::Error::Internal(format!("resolver failed: {e}")))
}
