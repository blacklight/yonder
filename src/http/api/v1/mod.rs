//! `/api/v1` — Mastodon-compatible REST API.
//!
//! Business routes land in epics 07–22: each adds its handlers here,
//! registers them on [`router`], and lists them in [`ApiDoc`]'s
//! `paths(...)`. Handlers carry `#[utoipa::path]`; keep them thin —
//! logic lives in domain services.

pub mod account_management;
pub mod accounts;
pub mod apps;
pub mod blocks;
pub mod custom_emojis;
pub mod emoji_admin;
pub mod follow_requests;
pub mod instance;
pub mod interactions;
pub mod invites;
pub mod lists;
pub mod media;
pub mod notifications;
pub mod polls;
pub mod relationships;
pub mod scheduled_statuses;
pub mod search;
pub mod sessions;
pub mod settings;
pub mod statuses;
pub mod tags;
pub mod timelines;
pub mod tokens;
pub mod transfer;
pub mod trends;
pub mod two_factor;

use std::sync::Arc;

use axum::{routing::get, routing::post, Router};

use crate::http::{middleware::ratelimit, AppState};

/// The v1 router. Merge feature subrouters here.
///
/// Layer order matters: `route_layer` wraps only the routes registered
/// *before* it — registration is added last so it counts solely against
/// `account_creation`, not both buckets.
pub fn router(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/accounts/verify_email",
            post(accounts::verify_email),
        )
        .route(
            "/api/v1/accounts/verify_credentials",
            get(accounts::verify_credentials),
        )
        // Profile editing (epic 09). Multipart bypasses
        // `DefaultBodyLimit`, so the body cap rides on the route itself
        // (two uploads + slack).
        .route(
            "/api/v1/accounts/update_credentials",
            axum::routing::patch(accounts::update_credentials).route_layer(
                tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_avatar_size as usize * 2 + 64 * 1024,
                ),
            ),
        )
        .route("/api/v1/apps", post(apps::create_app))
        .route(
            "/api/v1/apps/verify_credentials",
            get(apps::verify_credentials),
        )
        .route(
            "/api/v1/oauth/authorized_applications",
            get(apps::authorized_applications),
        )
        .route(
            "/api/v1/oauth/authorized_applications/{id}",
            axum::routing::delete(apps::revoke_authorization),
        )
        .route(
            "/api/v1/tokens",
            post(tokens::create_token).get(tokens::list_tokens),
        )
        .route(
            "/api/v1/tokens/{id}",
            axum::routing::delete(tokens::revoke_token),
        )
        .route("/api/v1/sessions", get(sessions::list_sessions))
        .route(
            "/api/v1/sessions/{id}",
            axum::routing::delete(sessions::revoke_session),
        )
        // 2FA management (epic 08) — inside the `authentication` bucket.
        .route(
            "/api/v1/2fa",
            get(two_factor::status).delete(two_factor::disable),
        )
        .route("/api/v1/2fa/totp/setup", post(two_factor::totp_setup))
        .route("/api/v1/2fa/totp/confirm", post(two_factor::totp_confirm))
        .route(
            "/api/v1/2fa/totp",
            axum::routing::delete(two_factor::totp_delete),
        )
        .route("/api/v1/2fa/backup_codes", post(two_factor::backup_codes))
        .route(
            "/api/v1/2fa/webauthn/register/begin",
            post(two_factor::webauthn_register_begin),
        )
        .route(
            "/api/v1/2fa/webauthn/register",
            post(two_factor::webauthn_register),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials",
            get(two_factor::webauthn_credentials),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials/{id}/rename",
            post(two_factor::webauthn_rename),
        )
        .route(
            "/api/v1/2fa/webauthn/credentials/{id}",
            axum::routing::delete(two_factor::webauthn_delete),
        )
        .route("/api/v1/instance", get(instance::instance))
        // User settings (epic 10).
        .route("/api/v1/preferences", get(settings::preferences))
        .route(
            "/api/v1/yonder/settings",
            get(settings::get_settings).patch(settings::patch_settings),
        )
        // Account management (epic 11).
        .route(
            "/api/v1/yonder/aliases",
            get(account_management::list_aliases)
                .post(account_management::add_alias)
                .delete(account_management::remove_alias),
        )
        .route(
            "/api/v1/yonder/move",
            post(account_management::move_account).delete(account_management::cancel_move),
        )
        .route(
            "/api/v1/yonder/account/delete",
            post(account_management::delete_account),
        )
        .route(
            "/api/v1/yonder/invites",
            post(invites::create_invite).get(invites::list_invites),
        )
        .route(
            "/api/v1/yonder/invites/{id}",
            axum::routing::delete(invites::revoke_invite),
        )
        .route(
            "/api/v1/yonder/exports",
            post(transfer::create_export).get(transfer::list_exports),
        )
        .route("/api/v1/yonder/exports/{id}", get(transfer::get_export))
        .route(
            "/api/v1/yonder/exports/{id}/download",
            get(transfer::download_export),
        )
        .route(
            "/api/v1/yonder/imports",
            post(transfer::create_import)
                .route_layer(tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.admin.max_import_size as usize + 64 * 1024,
                ))
                .get(transfer::list_imports),
        )
        .route("/api/v1/yonder/imports/{id}", get(transfer::get_import))
        // Posting (epic 12) — outside the `authentication` bucket;
        // writes carry their own buckets.
        .route(
            "/api/v1/statuses",
            axum::routing::post(statuses::create).route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}",
            get(statuses::show)
                .put(statuses::update)
                .delete(statuses::delete_status)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/media",
            axum::routing::post(media::upload)
                .route_layer(tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_upload_size as usize + 64 * 1024,
                ))
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route("/api/v1/media/{id}", axum::routing::put(media::update))
        // Interactions (epic 13) — writes carry the `status` bucket.
        .route(
            "/api/v1/statuses/{id}/favourite",
            axum::routing::post(interactions::favourite)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}/unfavourite",
            axum::routing::post(interactions::unfavourite)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route("/api/v1/favourites", get(interactions::favourites_list))
        .route(
            "/api/v1/statuses/{id}/favourited_by",
            get(interactions::favourited_by),
        )
        .route(
            "/api/v1/statuses/{id}/reblog",
            axum::routing::post(interactions::reblog)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}/unreblog",
            axum::routing::post(interactions::unreblog)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}/reblogged_by",
            get(interactions::reblogged_by),
        )
        .route(
            "/api/v1/statuses/{id}/bookmark",
            axum::routing::post(interactions::bookmark)
                .delete(interactions::unbookmark)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}/unbookmark",
            axum::routing::post(interactions::unbookmark)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route("/api/v1/bookmarks", get(interactions::bookmarks_list))
        .route(
            "/api/v1/statuses/{id}/pin",
            axum::routing::post(interactions::pin).route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/statuses/{id}/unpin",
            axum::routing::post(interactions::unpin).route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/pleroma/statuses/{id}/reactions/{emoji}",
            axum::routing::put(interactions::react)
                .post(interactions::react)
                .delete(interactions::unreact)
                .route_layer(ratelimit::layer(state, "status")),
        )
        .route(
            "/api/v1/pleroma/statuses/{id}/reactions",
            get(interactions::reactions_list),
        )
        // Social graph (epic 14) — reads are unlayered; writes carry
        // the `relation` bucket.
        .route(
            "/api/v1/accounts/relationships",
            get(relationships::relationships),
        )
        .route(
            "/api/v1/accounts/{id}/follow",
            post(relationships::follow)
                .delete(relationships::unfollow)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/unfollow",
            post(relationships::unfollow).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/follows",
            post(relationships::follow_by_uri).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/followers",
            get(relationships::followers),
        )
        .route(
            "/api/v1/accounts/{id}/following",
            get(relationships::following),
        )
        .route("/api/v1/accounts/{id}/lists", get(lists::account_lists))
        .route(
            "/api/v1/accounts/{id}/note",
            post(relationships::note).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/subscribe",
            post(relationships::subscribe_account)
                .delete(relationships::unsubscribe_account)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/unsubscribe",
            post(relationships::unsubscribe_account)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/follow_requests", get(follow_requests::list))
        .route(
            "/api/v1/follow_requests/{id}/authorize",
            post(follow_requests::authorize).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/follow_requests/{id}/reject",
            post(follow_requests::reject).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/follow_requests/{id}",
            axum::routing::delete(follow_requests::reject)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/block",
            post(blocks::block)
                .delete(blocks::unblock)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/unblock",
            post(blocks::unblock).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/mute",
            post(blocks::mute)
                .delete(blocks::unmute)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/accounts/{id}/unmute",
            post(blocks::unmute).route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/blocks", get(blocks::blocked))
        .route("/api/v1/mutes", get(blocks::muted))
        .route("/api/v1/domain_blocks", get(blocks::domain_blocks))
        .route(
            "/api/v1/domain_blocks",
            post(blocks::block_domain)
                .delete(blocks::unblock_domain)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/lists", get(lists::list))
        .route(
            "/api/v1/lists",
            post(lists::create).route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/lists/{id}", get(lists::show))
        .route(
            "/api/v1/lists/{id}",
            axum::routing::put(lists::update)
                .delete(lists::delete_list)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/lists/{id}/accounts", get(lists::members))
        .route(
            "/api/v1/lists/{id}/accounts",
            post(lists::add_accounts)
                .delete(lists::remove_accounts)
                .route_layer(ratelimit::layer(state, "relation")),
        )
        .route("/api/v1/polls/{id}", get(polls::show))
        .route(
            "/api/v1/polls/{id}/votes",
            axum::routing::post(polls::vote).route_layer(ratelimit::layer(state, "status")),
        )
        .route("/api/v1/scheduled_statuses", get(scheduled_statuses::list))
        .route(
            "/api/v1/scheduled_statuses/{id}",
            get(scheduled_statuses::show)
                .put(scheduled_statuses::update)
                .delete(scheduled_statuses::cancel),
        )
        // The `authentication` bucket covers every auth-adjacent
        // endpoint registered above.
        .route_layer(ratelimit::layer(state, "authentication"))
        // Registration gets only the dedicated `account_creation`
        // bucket — added after `route_layer` so it isn't double-counted.
        .route(
            "/api/v1/accounts",
            post(accounts::register).route_layer(ratelimit::layer(state, "account_creation")),
        )
        // Public invite check — outside the `authentication` bucket,
        // counted against `account_creation` like registration itself.
        .route(
            "/api/v1/yonder/invites/check/{code}",
            get(invites::check_invite).route_layer(ratelimit::layer(state, "account_creation")),
        )
        // Profile pages (epic 18) — public reads registered after the
        // `authentication` layer so they count solely against the
        // `timeline` read bucket, like the timeline endpoints below.
        .route(
            "/api/v1/accounts/{id}",
            get(accounts::get_account).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/accounts/{id}/statuses",
            get(accounts::account_statuses).route_layer(ratelimit::layer(state, "timeline")),
        )
        // Timelines (epic 15) — registered after the `authentication`
        // layer so reads count solely against the `timeline` bucket.
        .route(
            "/api/v1/timelines/home",
            get(timelines::home).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/timelines/public",
            get(timelines::public).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/timelines/list/{id}",
            get(timelines::list_timeline).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/timelines/tag/{hashtag}",
            get(timelines::tag).route_layer(ratelimit::layer(state, "timeline")),
        )
        // Epic 20 — hashtags and trends (timeline read bucket / relation writes).
        .route(
            "/api/v1/tags/{name}",
            get(tags::show).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/tags/{name}/follow",
            post(tags::follow).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/tags/{name}/unfollow",
            post(tags::unfollow).route_layer(ratelimit::layer(state, "relation")),
        )
        .route(
            "/api/v1/followed_tags",
            get(tags::followed_tags).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/trends/tags",
            get(trends::tags_trending).route_layer(ratelimit::layer(state, "timeline")),
        )
        // Epic 20 — custom emoji. Public catalogue on the timeline
        // bucket; admin surface on `media` (upload-shaped writes).
        .route(
            "/api/v1/custom_emojis",
            get(custom_emojis::list).route_layer(ratelimit::layer(state, "timeline")),
        )
        .route(
            "/api/v1/admin/emojis",
            post(emoji_admin::create)
                .route_layer(tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_emoji_size as usize + 64 * 1024,
                ))
                .get(emoji_admin::list)
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route(
            "/api/v1/admin/emojis/import_zip",
            post(emoji_admin::import_zip)
                .route_layer(tower_http::limit::RequestBodyLimitLayer::new(
                    state.config.media.max_emoji_zip_uncompressed as usize + 64 * 1024,
                ))
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route(
            "/api/v1/admin/emojis/import_dir",
            post(emoji_admin::import_dir).route_layer(ratelimit::layer(state, "media")),
        )
        .route(
            "/api/v1/admin/emojis/{id}",
            get(emoji_admin::show)
                .patch(emoji_admin::update)
                .delete(emoji_admin::delete_emoji)
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route(
            "/api/v1/admin/emoji_packs",
            post(emoji_admin::create_pack)
                .get(emoji_admin::list_packs)
                .route_layer(ratelimit::layer(state, "media")),
        )
        .route(
            "/api/v1/admin/emoji_packs/{id}",
            axum::routing::patch(emoji_admin::update_pack)
                .delete(emoji_admin::delete_pack)
                .route_layer(ratelimit::layer(state, "media")),
        )
        // Epic 16 — notifications (no rate-limit bucket; Mastodon
        // doesn't throttle these).
        .route("/api/v1/notifications", get(notifications::list))
        .route("/api/v1/notifications/clear", post(notifications::clear))
        .route(
            "/api/v1/notifications/unread_count",
            get(notifications::unread_count),
        )
        .route("/api/v1/notifications/{id}", get(notifications::show))
        .route(
            "/api/v1/notifications/{id}/dismiss",
            post(notifications::dismiss),
        )
        // Epic 17 — search (own `search` bucket, like timelines).
        .route(
            "/api/v1/search",
            get(search::search_v1).route_layer(ratelimit::layer(state, "search")),
        )
}

/// OpenAPI fragment merged into the root document.
#[derive(utoipa::OpenApi)]
#[openapi(paths(
    accounts::register,
    accounts::verify_credentials,
    accounts::update_credentials,
    accounts::verify_email,
    accounts::get_account,
    accounts::account_statuses,
    apps::create_app,
    apps::verify_credentials,
    apps::authorized_applications,
    apps::revoke_authorization,
    tokens::create_token,
    tokens::list_tokens,
    tokens::revoke_token,
    sessions::list_sessions,
    sessions::revoke_session,
    two_factor::status,
    two_factor::totp_setup,
    two_factor::totp_confirm,
    two_factor::totp_delete,
    two_factor::backup_codes,
    two_factor::webauthn_register_begin,
    two_factor::webauthn_register,
    two_factor::webauthn_credentials,
    two_factor::webauthn_rename,
    two_factor::webauthn_delete,
    two_factor::disable,
    instance::instance,
    settings::preferences,
    settings::get_settings,
    settings::patch_settings,
    account_management::list_aliases,
    account_management::add_alias,
    account_management::remove_alias,
    account_management::move_account,
    account_management::cancel_move,
    account_management::delete_account,
    invites::create_invite,
    invites::list_invites,
    invites::revoke_invite,
    invites::check_invite,
    transfer::create_export,
    transfer::list_exports,
    transfer::get_export,
    transfer::download_export,
    transfer::create_import,
    transfer::list_imports,
    transfer::get_import,
    statuses::create,
    statuses::show,
    statuses::update,
    statuses::delete_status,
    interactions::favourite,
    interactions::unfavourite,
    interactions::favourites_list,
    interactions::favourited_by,
    interactions::reblog,
    interactions::unreblog,
    interactions::reblogged_by,
    interactions::bookmark,
    interactions::unbookmark,
    interactions::bookmarks_list,
    interactions::pin,
    interactions::unpin,
    interactions::react,
    relationships::follow,
    relationships::unfollow,
    relationships::follow_by_uri,
    relationships::relationships,
    relationships::followers,
    relationships::following,
    relationships::note,
    relationships::subscribe_account,
    relationships::unsubscribe_account,
    follow_requests::list,
    follow_requests::authorize,
    follow_requests::reject,
    blocks::block,
    blocks::unblock,
    blocks::mute,
    blocks::unmute,
    blocks::blocked,
    blocks::muted,
    blocks::domain_blocks,
    blocks::block_domain,
    blocks::unblock_domain,
    lists::list,
    lists::create,
    lists::show,
    lists::update,
    lists::delete_list,
    lists::members,
    lists::add_accounts,
    lists::remove_accounts,
    lists::account_lists,
    interactions::unreact,
    interactions::reactions_list,
    media::upload,
    media::update,
    polls::show,
    polls::vote,
    timelines::home,
    timelines::public,
    timelines::list_timeline,
    timelines::tag,
    notifications::list,
    notifications::show,
    notifications::clear,
    notifications::dismiss,
    notifications::unread_count,
    scheduled_statuses::list,
    scheduled_statuses::show,
    scheduled_statuses::update,
    scheduled_statuses::cancel,
    search::search_v1,
    tags::show,
    tags::follow,
    tags::unfollow,
    tags::followed_tags,
    trends::tags_trending,
    custom_emojis::list,
    emoji_admin::create,
    emoji_admin::list,
    emoji_admin::show,
    emoji_admin::update,
    emoji_admin::delete_emoji,
    emoji_admin::import_zip,
    emoji_admin::import_dir,
    emoji_admin::create_pack,
    emoji_admin::list_packs,
    emoji_admin::update_pack,
    emoji_admin::delete_pack
))]
pub struct ApiDoc;
