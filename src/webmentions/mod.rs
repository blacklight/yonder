//! W3C Webmention send/receive — the IndieWeb counterpart of the
//! ActivityPub mention pipeline, backed by `webmentions-sdk`.
//!
//! **Incoming** (`POST /webmentions`, see [`incoming`]): remote sites
//! POST `source`/`target` form fields; the endpoint validates and
//! enqueues a `WebmentionIncoming` job (HTTP 202). The job runs the
//! SDK's processor — source fetch under SSRF guard, backlink
//! verification, Microformats2 parse, ledger store — then
//! materializes the mention as a `statuses` reply authored by a
//! **pseudo-actor**: an `accounts` row keyed on the author's site
//! (`urn:yonder:webmention:…` actor id, never dereferenced over AP).
//! Source edits update the reply; source removals retract it.
//!
//! **Outgoing** (`GET /webmentions/source/{id}` + `WebmentionOutgoing`,
//! see [`outgoing`]): publishing or editing a local public/unlisted
//! status whose content carries URLs enqueues the outgoing job. The
//! SDK extracts link targets from the status's rendered `h-entry`
//! page, discovers each target's Webmention endpoint (`Link` header or
//! `<link rel="webmention">`) and POSTs the mention. Stored outgoing
//! rows diff against the new target set so edits and deletions
//! retract delivered mentions.
//!
//! The endpoint is advertised with a `Link: <…>; rel="webmention"`
//! response header injected on `text/*` responses (see
//! [`inject_link_header`]) and a `<link>` element on the source
//! page itself.
//!
//! The SDK is synchronous (ureq) — every call into it runs inside
//! [`crate::activitypub::actors::sync_call`], the same
//! `block_in_place` bridge the pubby handlers use. The storage trait
//! it expects is sync too, so [`storage::PgWebmentionsStorage`]
//! bridges with `block_on` over the sqlx pool.

pub mod incoming;
pub mod outgoing;
pub mod render;
pub mod storage;

use std::sync::Arc;

use axum::Router;
use sha2::{Digest, Sha256};

use crate::{activitypub::federation_base_url, config::Config, http::AppState};

/// Whether Webmention support is on and a public domain is configured
/// (the endpoint/source URLs need a canonical base).
pub fn enabled(config: &Config) -> bool {
    config.webmentions.enabled && !config.instance.domain.trim().is_empty()
}

/// The public URL of the incoming Webmention endpoint.
pub fn endpoint_url(config: &Config) -> String {
    format!("{}/webmentions", federation_base_url(config))
}

/// The public URL of a status's outgoing-mention source page — what
/// remote receivers fetch to verify (and render) our mention.
pub fn source_url(config: &Config, status_id: i64) -> String {
    format!(
        "{}/webmentions/source/{status_id}",
        federation_base_url(config)
    )
}

/// The public URL asserting `account_id`'s like of `status_id` — the
/// source remote receivers fetch to verify `like-of` backfeeds.
pub fn like_source_url(config: &Config, status_id: i64, account_id: i64) -> String {
    format!(
        "{}/webmentions/like/{status_id}/{account_id}",
        federation_base_url(config)
    )
}

/// The deterministic pseudo-actor `actor_id` for a Webmention author
/// identity (`author_url`, falling back to the source's origin).
/// Non-HTTP so it is never dereferenced by the remote-actor pipeline.
pub fn actor_id_for(author_key: &str) -> String {
    format!("urn:yonder:webmention:{:.40}", hex_sha256(author_key))
}

/// The deterministic reply-status `uri` for a (source, target) pair —
/// the `ON CONFLICT (uri)` upsert key that makes re-verified mentions
/// idempotent.
pub fn status_uri_for(source: &str, target: &str) -> String {
    format!(
        "urn:yonder:webmention:status:{:.40}",
        hex_sha256(&format!("{source}|{target}"))
    )
}

fn hex_sha256(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(digest)
}

/// Build the SDK handler over the Postgres storage, mirroring
/// Songhive's `create_webmentions_handler`: SSRF protection stays on
/// (every fetched URL — each redirect hop included — is revalidated
/// against non-public addresses and bodies are capped), local targets
/// are excluded from outgoing delivery (same-instance mentions already
/// flow through the regular reply pipeline), and the callbacks feed
/// the materialization side.
pub fn build_handler(
    config: &Config,
    storage: Arc<storage::PgWebmentionsStorage>,
    on_mention_processed: Option<webmentions_sdk::MentionCallback>,
    on_mention_deleted: Option<webmentions_sdk::MentionCallback>,
) -> webmentions_sdk::WebmentionsHandler {
    let base_url = federation_base_url(config);
    webmentions_sdk::WebmentionsHandler::new(
        storage,
        webmentions_sdk::HandlerOptions {
            base_url: Some(base_url.clone()),
            base_urls: vec![base_url],
            http_timeout: config.webmentions.request_timeout_seconds as f64,
            max_discovery_response_bytes: config.webmentions.discovery_max_bytes,
            max_source_response_bytes: config.webmentions.source_max_bytes,
            ssrf_protection: !(config.webmentions.allow_private_targets
                || config.federation.allow_private_targets),
            exclude_local_targets: true,
            user_agent: config.federation.user_agent.clone(),
            on_mention_processed,
            on_mention_deleted,
            ..Default::default()
        },
    )
}

/// The webmention routes — merged into the app router when
/// [`enabled`]. `POST /webmentions` is rate-limited (unauthenticated).
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    use axum::routing::{get, post};
    Router::new()
        .route("/webmentions", post(incoming::receive))
        .route_layer(crate::http::middleware::ratelimit::layer(
            state,
            "webmention",
        ))
        .route("/webmentions/source/{id}", get(outgoing::serve_source))
        .route(
            "/webmentions/like/{status_id}/{account_id}",
            get(outgoing::serve_like),
        )
}

/// Router-level middleware advertising the endpoint on every `text/*`
/// response — how remote senders discover the receiver when they scan
/// the page they linked to (SPA status permalinks, feeds and the
/// `h-entry` source page itself all carry `text/*` content types).
/// Mounted via `axum::middleware::from_fn_with_state`.
pub async fn inject_link_header(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let is_text = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .starts_with("text/")
        })
        .unwrap_or(false);
    if is_text {
        let link =
            webmentions_sdk::server::webmention_link_header_value(&endpoint_url(&state.config()));
        if let Ok(value) = axum::http::HeaderValue::from_str(&link) {
            resp.headers_mut().append(axum::http::header::LINK, value);
        }
    }
    resp
}

/// Register the job handlers (called by [`crate::cli::build_registry`]).
/// `ap` is optional — materialization only needs it for the streaming
/// bus and timeline fan-out context.
pub fn register_jobs(
    registry: &mut crate::jobs::Registry,
    ap: Option<&crate::activitypub::ApContext>,
) {
    registry.register(
        crate::jobs::JobKind::WebmentionIncoming,
        incoming::IncomingJob::new(ap.cloned()),
    );
    registry.register(
        crate::jobs::JobKind::WebmentionOutgoing,
        outgoing::OutgoingJob::new(ap.cloned()),
    );
}
