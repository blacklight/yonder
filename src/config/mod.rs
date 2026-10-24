//! Layered instance configuration.
//!
//! Priority, highest wins:
//!
//! 1. database overlays (`config_overlays` table, set via admin panel/CLI)
//! 2. environment variables (`YONDER_` prefix, `__` as nesting separator)
//! 3. `config.toml`
//! 4. built-in defaults
//!
//! The DB layer is resolved in two passes ([`Config::load_full`]): TOML + env
//! produce the database connection settings, a guarded read of
//! `config_overlays` yields the top layer, and the figment is re-resolved
//! into the effective [`Config`].
//!
//! [`registry`] maps every leaf to both its TOML path (`rate_limit.search.limit`)
//! and its env-var name (`YONDER_RATE_LIMIT__SEARCH__LIMIT`) — the contract the
//! admin panel (epic 22) and `yonder config` (epic 27) consume.

pub mod de;
pub mod overlays;
pub mod registry;

use std::path::{Path, PathBuf};

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{db, Result};

/// Environment variable prefix for all configuration overrides.
pub const ENV_PREFIX: &str = "YONDER_";

/// Config file looked up when neither `--config` nor `YONDER_CONFIG` is set.
pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Environment variable naming the config file.
pub const CONFIG_PATH_ENV: &str = "YONDER_CONFIG";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub instance: InstanceConfig,
    pub i18n: I18nConfig,
    pub auth: AuthConfig,
    pub federation: FederationConfig,
    pub media: MediaConfig,
    pub email: EmailConfig,
    pub search: SearchConfig,
    pub http: HttpConfig,
    pub rate_limit: RateLimitConfig,
    pub polls: PollsConfig,
    pub content: ContentConfig,
    pub admin: AdminConfig,
    pub jobs: JobsConfig,
    pub timelines: TimelinesConfig,
}

impl Config {
    /// Resolve the config file path: explicit argument wins, then
    /// `YONDER_CONFIG`, then `config.toml` in the working directory.
    pub fn config_path(path: Option<&Path>) -> PathBuf {
        path.map(PathBuf::from)
            .or_else(|| std::env::var_os(CONFIG_PATH_ENV).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
    }

    /// First-pass resolution: defaults + TOML + environment.
    ///
    /// A missing config file is not an error — defaults and env vars alone
    /// must be able to boot the process.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        Self::figment(path).extract().map_err(Into::into)
    }

    /// Re-resolve with database overlays merged on top of the other layers.
    ///
    /// `overlay` keys are dotted TOML paths (`"instance.name"`), matching the
    /// rows stored in `config_overlays`.
    pub fn with_overlays(
        path: Option<&Path>,
        overlays: &[(String, serde_json::Value)],
    ) -> Result<Self> {
        Self::figment(path)
            .merge(overlays::provider(overlays))
            .extract()
            .map_err(Into::into)
    }

    /// Full two-pass resolution: load TOML + env, connect to Postgres, read
    /// `config_overlays` if present, and re-resolve. Returns the effective
    /// config together with the pool used to fetch the overlays.
    pub async fn load_full(path: Option<&Path>) -> Result<(Self, PgPool)> {
        let preliminary = Self::load(path)?;
        let pool = db::connect(&preliminary.database).await?;
        let overlays = db::config_overlays::fetch(&pool).await?;
        let config = if overlays.is_empty() {
            preliminary
        } else {
            Self::with_overlays(path, &overlays)?
        };
        config.validate()?;
        Ok((config, pool))
    }

    /// Cheap semantic validation of the resolved config — catches
    /// combinations that deserialize fine but break at runtime.
    pub fn validate(&self) -> Result<()> {
        for name in [
            "search",
            "feed",
            "timeline",
            "account_creation",
            "relation",
            "status",
            "media",
            "authentication",
            "federation_inbox",
        ] {
            let limit = self.rate_limit.bucket(name).expect("static bucket name");
            if limit.limit == 0 || limit.period_seconds == 0 {
                return Err(crate::Error::Validation(format!(
                    "rate_limit.{name}: limit and period_seconds must be > 0"
                )));
            }
        }
        if self.jobs.handler_timeout_seconds >= self.jobs.reclaim_idle_seconds {
            return Err(crate::Error::Validation(
                "jobs.handler_timeout_seconds must be < jobs.reclaim_idle_seconds \
                 (otherwise a still-running job is reclaimed and executed twice)"
                    .to_string(),
            ));
        }
        if self.admin.account_deletion_grace_days > 365 {
            return Err(crate::Error::Validation(
                "admin.account_deletion_grace_days must be <= 365".to_string(),
            ));
        }
        if self.jobs.poll_interval_ms == 0 {
            return Err(crate::Error::Validation(
                "jobs.poll_interval_ms must be > 0".to_string(),
            ));
        }
        if !(1..=200).contains(&self.content.max_feed_items) {
            return Err(crate::Error::Validation(
                "content.max_feed_items must be in 1..=200".to_string(),
            ));
        }
        if !(1..=1000).contains(&self.content.max_filter_keywords) {
            return Err(crate::Error::Validation(
                "content.max_filter_keywords must be in 1..=1000".to_string(),
            ));
        }
        if !(1..=5000).contains(&self.content.max_filters) {
            return Err(crate::Error::Validation(
                "content.max_filters must be in 1..=5000".to_string(),
            ));
        }
        if !(1..=100).contains(&self.timelines.trends_max_limit) {
            return Err(crate::Error::Validation(
                "timelines.trends_max_limit must be in 1..=100".to_string(),
            ));
        }
        if self.timelines.trends_cache_seconds > 86400 {
            return Err(crate::Error::Validation(
                "timelines.trends_cache_seconds must be <= 86400".to_string(),
            ));
        }
        if !(1024..=10_485_760).contains(&self.media.max_emoji_size) {
            return Err(crate::Error::Validation(
                "media.max_emoji_size must be in 1024..=10485760".to_string(),
            ));
        }
        if !(1_048_576..=1_073_741_824).contains(&self.media.max_emoji_zip_uncompressed) {
            return Err(crate::Error::Validation(
                "media.max_emoji_zip_uncompressed must be in 1048576..=1073741824".to_string(),
            ));
        }
        if self.timelines.home_max_entries == 0
            || self.timelines.fanout_batch_size == 0
            || self.timelines.rebuild_page_size == 0
        {
            return Err(crate::Error::Validation(
                "timelines.home_max_entries, timelines.fanout_batch_size and \
                 timelines.rebuild_page_size must all be > 0"
                    .to_string(),
            ));
        }
        if !crate::i18n::SUPPORTED_LOCALES.contains(&self.i18n.fallback_locale.as_str()) {
            return Err(crate::Error::Validation(format!(
                "i18n.fallback_locale must be one of: {}",
                crate::i18n::SUPPORTED_LOCALES.join(", ")
            )));
        }
        Ok(())
    }

    fn figment(path: Option<&Path>) -> Figment {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        let path = Self::config_path(path);
        if path.is_file() {
            figment = figment.merge(Toml::file(path));
        }
        figment.merge(Env::prefixed(ENV_PREFIX).split("__"))
    }
}

/// `server.log_format` values.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, multi-line span/field rendering.
    #[default]
    Pretty,
    /// Single-line compact output.
    Compact,
    /// JSON objects, for log aggregation.
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// Filter directive for `tracing_subscriber::EnvFilter`
    /// (e.g. `info`, `debug`, `yonder=debug,sqlx=warn`). `RUST_LOG`
    /// overrides this when set.
    pub log_level: String,
    pub log_format: LogFormat,
    /// Number of trusted reverse-proxy hops used when resolving the real
    /// client IP from `X-Forwarded-For`.
    pub trusted_proxy_hops: u16,
    /// Run pending schema migrations on startup. Disable to manage
    /// migrations explicitly via `yonder migrate`.
    pub migrate_on_start: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 3000,
            log_level: "info".to_string(),
            log_format: LogFormat::Pretty,
            trusted_proxy_hops: 0,
            migrate_on_start: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    /// Seconds to wait for a pooled connection before failing.
    pub acquire_timeout_seconds: u64,
    /// Seconds an idle pooled connection is kept.
    pub idle_timeout_seconds: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgres://yonder:yonder@localhost:5432/yonder".to_string(),
            max_connections: 10,
            min_connections: 1,
            acquire_timeout_seconds: 5,
            idle_timeout_seconds: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RedisConfig {
    pub url: String,
    /// Maximum number of connections in the async pool.
    pub pool_size: u32,
    /// Prefix for every Redis key Yonder writes (`{key_prefix}:…`).
    pub key_prefix: String,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379/0".to_string(),
            pool_size: 10,
            key_prefix: "yonder".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstanceConfig {
    /// Public domain name of this instance (`example.com`, no scheme).
    pub domain: String,
    pub name: String,
    pub description: String,
    pub short_description: String,
    /// BCP-47 languages the instance primarily operates in.
    #[serde(deserialize_with = "de::string_or_seq")]
    pub languages: Vec<String>,
    pub contact_name: Option<String>,
    pub contact_email: Option<String>,
    pub contact_url: Option<String>,
    pub logo_path: Option<String>,
    pub banner_path: Option<String>,
    /// Server rules shown on the /about page.
    #[serde(deserialize_with = "de::string_or_seq")]
    pub rules: Vec<String>,
    /// Publish the list of moderated instances over API/UI.
    pub expose_moderated_instances: bool,
    /// Enable the profile directory.
    pub directory_enabled: bool,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            domain: "localhost".to_string(),
            name: "Yonder".to_string(),
            description: String::new(),
            short_description: String::new(),
            languages: vec!["en".to_string()],
            contact_name: None,
            contact_email: None,
            contact_url: None,
            logo_path: None,
            banner_path: None,
            rules: Vec::new(),
            expose_moderated_instances: true,
            directory_enabled: true,
        }
    }
}

/// Interface-language settings (BOOTSTRAP i18n set: en/es/it/de/ar).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct I18nConfig {
    /// Locale reported when neither the user's stored locale nor the
    /// client's `Accept-Language` is a supported locale. Must be one of
    /// the supported codes — validated in `Config::validate`.
    pub fallback_locale: String,
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            fallback_locale: "en".to_string(),
        }
    }
}

/// `auth.registration_mode` values.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationMode {
    /// Everyone can sign up; posting requires email verification when a mail
    /// server is configured.
    #[default]
    Open,
    /// Sign-ups are accepted but must be approved by an admin/moderator.
    ApprovalRequired,
    /// Registrations only via invite links or admin-side creation.
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub registration_mode: RegistrationMode,
    /// Allow non-admin/mod users to generate invite links. Unset means
    /// "enabled when registrations are open, disabled otherwise".
    pub invite_links_enabled: Option<bool>,
    /// Custom message shown in approval-required registration forms
    /// (≤ `content.max_registration_message_length`).
    pub registration_message: Option<String>,
    pub require_email_verification: bool,
    /// Signing secret for session/token material. Prefer
    /// `YONDER_AUTH__SECRET_KEY` over the TOML file.
    pub secret_key: Option<String>,
    pub access_token_expiry_minutes: u64,
    pub refresh_token_expiry_days: u64,
}

impl AuthConfig {
    /// The effective invite-link policy for non-staff users: an explicit
    /// value wins; unset means "on only while registration is open".
    pub fn invite_links_enabled(&self) -> bool {
        self.invite_links_enabled
            .unwrap_or(matches!(self.registration_mode, RegistrationMode::Open))
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            registration_mode: RegistrationMode::Open,
            invite_links_enabled: None,
            registration_message: None,
            require_email_verification: true,
            secret_key: None,
            access_token_expiry_minutes: 120,
            refresh_token_expiry_days: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FederationConfig {
    /// Master switch for ActivityPub federation.
    pub enabled: bool,
    /// When false, instance content requires authentication to view.
    pub public: bool,
    /// Allow remote relays to subscribe to all public posts.
    pub allow_relays: bool,
    /// Sign outbound object fetches with the instance HTTP signature.
    pub sign_fetches: bool,
    /// Require HTTP signatures on inbound ActivityPub fetches.
    pub authorized_fetch: bool,
    /// Public base URL for ActivityPub identifiers (scheme included,
    /// e.g. `https://example.com`). Defaults to `https://{instance.domain}`
    /// at resolve time — set explicitly for http dev and containerized
    /// federation tests.
    pub base_url: Option<String>,
    /// Allow federation traffic to loopback/private targets. Dev/test
    /// only — the SSRF guard refuses them otherwise.
    pub allow_private_targets: bool,
    /// Maximum depth of incoming federated reply threads to fetch.
    pub max_reply_depth: u32,
    pub follow_handshake_timeout_seconds: u64,
    /// Timeout for the initiation of outbound HTTP requests.
    pub request_timeout_seconds: u64,
    /// Timeout for the completion of outbound HTTP requests.
    pub receive_timeout_seconds: u64,
    /// Number of outbound HTTP connections to keep open.
    pub pool_size: u32,
    /// Optional proxy for outbound federation traffic.
    pub proxy_url: Option<String>,
    /// User-Agent for outbound federation requests.
    pub user_agent: String,
    /// Maximum number of items to fetch from a remote collection.
    pub max_collection_objects: u32,
    /// Remote activities older than this are pruned when no local user has
    /// interacted with them.
    pub remote_activity_retention_days: u32,
    /// Optional cron expression scheduling remote-activity pruning.
    pub remote_activity_prune_schedule: Option<String>,
    /// Cron expression for the delivery-ledger reconciler — re-enqueues
    /// `deliveries` rows still `pending` past `next_attempt_at` (a lost
    /// `FederationDelivery` job: an enqueue failure or a crash between
    /// the ledger insert and the enqueue). Set to `""` to disable.
    pub delivery_sweep_schedule: Option<String>,
    /// Cron expression for the profile-link `rel="me"` re-verification
    /// sweep (`VerifyProfileLinks` over every local account with
    /// URL-valued fields). Set to `""` to disable; edits always enqueue
    /// their own verification regardless of this schedule.
    pub profile_link_verification_schedule: Option<String>,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            public: true,
            allow_relays: true,
            sign_fetches: true,
            authorized_fetch: false,
            base_url: None,
            allow_private_targets: false,
            max_reply_depth: 100,
            follow_handshake_timeout_seconds: 30,
            request_timeout_seconds: 10,
            receive_timeout_seconds: 30,
            pool_size: 10,
            proxy_url: None,
            user_agent: format!("Yonder/{}", env!("CARGO_PKG_VERSION")),
            max_collection_objects: 50,
            remote_activity_retention_days: 30,
            remote_activity_prune_schedule: None,
            delivery_sweep_schedule: Some("*/15 * * * *".to_string()),
            profile_link_verification_schedule: Some("0 4 * * *".to_string()),
        }
    }
}

/// `media.backend` values.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaBackend {
    /// Store uploads on the local filesystem under `media.local_path`.
    #[default]
    Local,
    /// Store uploads on an S3-compatible object store.
    S3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub backend: MediaBackend,
    pub local_path: String,
    pub s3_endpoint: Option<String>,
    pub s3_bucket: Option<String>,
    /// Prefer `YONDER_MEDIA__S3_ACCESS_KEY` / `YONDER_MEDIA__S3_SECRET_KEY`
    /// for credentials.
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub s3_region: Option<String>,
    /// Path-style bucket addressing — MinIO and most S3-compatible
    /// stores need it (`true`); AWS S3 accepts either.
    pub s3_force_path_style: bool,
    /// Optional public/CDN base URL media is linked from directly.
    /// When unset, media is proxied through `GET /media/{key}`.
    pub s3_public_base_url: Option<String>,
    /// Maximum upload size in bytes (default: 50 MiB).
    pub max_upload_size: u64,
    /// Maximum avatar/banner upload size in bytes (default: 2 MiB).
    pub max_avatar_size: u64,
    /// Fetch and cache remote media attachments instead of serving them
    /// from the remote instance.
    pub fetch_remote_media: bool,
    /// Optional cron expression scheduling remote-media cleanup.
    pub remote_cleanup_schedule: Option<String>,
    /// Cached remote media above this total size becomes eligible for
    /// cleanup (bytes).
    pub remote_cleanup_max_bytes: u64,
    /// Cached remote media older than this becomes eligible for cleanup.
    pub remote_cleanup_max_age_days: u32,
    /// Strip EXIF metadata from uploaded media.
    pub strip_exif: bool,
    /// Maximum custom-emoji file size in bytes (default: 256 KiB).
    pub max_emoji_size: u64,
    /// Cumulative uncompressed-size cap for emoji zip imports
    /// (default: 50 MiB) — the zip-bomb bound.
    pub max_emoji_zip_uncompressed: u64,
    /// Maximum media attachments per post.
    pub max_attachments_per_post: u32,
    /// Unattached uploads older than this are deleted by the orphan
    /// cleanup (days). Media reserved by a scheduled status is never
    /// orphaned.
    pub orphan_retention_days: u32,
    /// Cron expression for the orphan-media cleanup. Set to `""` to
    /// disable.
    pub orphan_cleanup_schedule: Option<String>,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            backend: MediaBackend::Local,
            local_path: "./media".to_string(),
            s3_endpoint: None,
            s3_bucket: None,
            s3_access_key: None,
            s3_secret_key: None,
            s3_region: None,
            s3_force_path_style: true,
            s3_public_base_url: None,
            max_upload_size: 50 * 1024 * 1024,
            max_avatar_size: 2 * 1024 * 1024,
            fetch_remote_media: false,
            remote_cleanup_schedule: None,
            remote_cleanup_max_bytes: 10 * 1024 * 1024 * 1024,
            remote_cleanup_max_age_days: 7,
            strip_exif: true,
            max_emoji_size: 256 * 1024,
            max_emoji_zip_uncompressed: 50 * 1024 * 1024,
            max_attachments_per_post: 4,
            orphan_retention_days: 7,
            orphan_cleanup_schedule: Some("30 3 * * *".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    /// Outgoing mail server. Unset means email is disabled.
    pub smtp_host: Option<String>,
    pub smtp_port: u16,
    pub smtp_username: Option<String>,
    /// Prefer `YONDER_EMAIL__SMTP_PASSWORD` over the TOML file.
    pub smtp_password: Option<String>,
    /// Use STARTTLS/TLS for the SMTP connection.
    pub smtp_tls: bool,
    pub from_address: String,
    /// Send a welcome email to newly registered users.
    pub send_welcome: bool,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            smtp_host: None,
            smtp_port: 587,
            smtp_username: None,
            smtp_password: None,
            smtp_tls: true,
            from_address: "yonder@localhost".to_string(),
            send_welcome: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Allow unauthenticated users to search.
    pub unauthenticated_enabled: bool,
    /// Fetch remote results (actors, posts) when a search doesn't match
    /// locally.
    pub remote_fetch: bool,
    /// Optional Elasticsearch/OpenSearch URL for indexed search.
    pub elasticsearch_url: Option<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            unauthenticated_enabled: true,
            remote_fetch: true,
            elasticsearch_url: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    /// Enable CORS. Disabled by default — enable only with an explicit
    /// origin allow-list.
    pub cors_enabled: bool,
    /// Allowed CORS origins (comma-separated via env).
    #[serde(deserialize_with = "de::string_or_seq")]
    pub cors_origins: Vec<String>,
    /// `mailto:` contact embedded in WebPush requests.
    pub webpush_subject: Option<String>,
    /// Prefer `YONDER_HTTP__VAPID_PRIVATE_KEY` for the private key.
    pub vapid_public_key: Option<String>,
    pub vapid_private_key: Option<String>,
}

/// One rate limit: `limit` requests per `period_seconds`, with an optional
/// higher allowance for authenticated users.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimit {
    pub limit: u32,
    pub period_seconds: u64,
    /// Override applied to authenticated requests; unset means `limit`
    /// applies to everyone.
    pub authenticated_limit: Option<u32>,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            limit: 10,
            period_seconds: 60,
            authenticated_limit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub search: RateLimit,
    /// RSS/Atom feed endpoints (`/users/*.atom|.rss`, `/tags/*`,
    /// `/lists/*`).
    pub feed: RateLimit,
    pub timeline: RateLimit,
    pub account_creation: RateLimit,
    /// Follow/unfollow requests.
    pub relation: RateLimit,
    /// Status create/update/delete.
    pub status: RateLimit,
    /// Media uploads (`/api/v1/media`, `/api/v2/media`).
    pub media: RateLimit,
    pub authentication: RateLimit,
    /// Inbound ActivityPub inbox POSTs.
    pub federation_inbox: RateLimit,
}

impl RateLimitConfig {
    /// Look up a named bucket (`"search"`, `"feed"`, `"timeline"`,
    /// `"account_creation"`, `"relation"`, `"status"`, `"media"`,
    /// `"authentication"`, `"federation_inbox"`).
    pub fn bucket(&self, name: &str) -> Option<&RateLimit> {
        match name {
            "search" => Some(&self.search),
            "feed" => Some(&self.feed),
            "timeline" => Some(&self.timeline),
            "account_creation" => Some(&self.account_creation),
            "relation" => Some(&self.relation),
            "status" => Some(&self.status),
            "media" => Some(&self.media),
            "authentication" => Some(&self.authentication),
            "federation_inbox" => Some(&self.federation_inbox),
            _ => None,
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            search: RateLimit {
                limit: 20,
                period_seconds: 60,
                authenticated_limit: None,
            },
            feed: RateLimit {
                limit: 30,
                period_seconds: 60,
                authenticated_limit: None,
            },
            timeline: RateLimit {
                limit: 5,
                period_seconds: 10,
                authenticated_limit: None,
            },
            account_creation: RateLimit {
                limit: 5,
                period_seconds: 60,
                authenticated_limit: None,
            },
            relation: RateLimit {
                limit: 10,
                period_seconds: 60,
                authenticated_limit: None,
            },
            status: RateLimit {
                limit: 15,
                period_seconds: 60,
                authenticated_limit: None,
            },
            media: RateLimit {
                limit: 30,
                period_seconds: 300,
                authenticated_limit: None,
            },
            authentication: RateLimit {
                limit: 5,
                period_seconds: 60,
                authenticated_limit: None,
            },
            federation_inbox: RateLimit {
                limit: 300,
                period_seconds: 300,
                authenticated_limit: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PollsConfig {
    pub max_options: u32,
    pub max_option_chars: u32,
    pub min_duration_seconds: u64,
    /// Default: 30 days.
    pub max_duration_seconds: u64,
}

impl Default for PollsConfig {
    fn default() -> Self {
        Self {
            max_options: 10,
            max_option_chars: 200,
            min_duration_seconds: 5 * 60,
            max_duration_seconds: 30 * 24 * 60 * 60,
        }
    }
}

/// `content.translation_backend` values.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TranslationBackend {
    /// Google Translate via the frontend scraper endpoint.
    #[default]
    GoogleWeb,
    /// Google Translate API (requires `google_translate_api_key`).
    GoogleApi,
    /// DeepL API (requires `deepl_api_key`).
    DeepL,
    /// Self-hosted LibreTranslate (requires `libretranslate_url`).
    LibreTranslate,
}

/// `content.default_post_content_type` values.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum PostContentType {
    #[default]
    #[serde(rename = "text/plain")]
    TextPlain,
    #[serde(rename = "text/markdown")]
    TextMarkdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentConfig {
    pub max_post_length: u32,
    pub max_bio_length: u32,
    /// Display-name cap — deliberately under the `accounts.display_name`
    /// CHECK of 500.
    pub max_display_name_length: u32,
    pub max_profile_fields: u32,
    pub max_profile_field_name_length: u32,
    pub max_profile_field_value_length: u32,
    pub max_username_length: u32,
    pub max_registration_message_length: u32,
    pub max_report_comment: u32,
    /// Maximum number of items in RSS/Atom feeds.
    pub max_feed_items: u32,
    /// Maximum number of keywords per filter (epic 20).
    pub max_filter_keywords: u32,
    /// Maximum number of filters per account (epic 20) — the `hide`
    /// clause cost scales with it on every filtered read.
    pub max_filters: u32,
    /// Maximum number of pinned posts per account.
    pub max_pinned_posts: u32,
    /// URL schemes accepted in posts, profile fields and link preview fetches.
    #[serde(deserialize_with = "de::string_or_seq")]
    pub valid_url_schemes: Vec<String>,
    pub translation_backend: TranslationBackend,
    /// Prefer env vars for API keys (`YONDER_CONTENT__GOOGLE_TRANSLATE_API_KEY`,
    /// `YONDER_CONTENT__DEEPL_API_KEY`).
    pub google_translate_api_key: Option<String>,
    pub deepl_api_key: Option<String>,
    pub libretranslate_url: Option<String>,
    pub default_post_content_type: PostContentType,
    /// Fetch Open Graph metadata for the first link of a status.
    pub link_previews: bool,
    /// Redis TTL for per-URL link-preview metadata (hours).
    pub link_preview_cache_hours: u32,
    /// Cron expression re-enqueueing due scheduled statuses (lost-job
    /// recovery). Set to `""` to disable.
    pub scheduled_status_sweep_schedule: Option<String>,
    /// Cron expression enqueueing due scheduled status deletions
    /// (`users.scheduled_deletion_days`). Set to `""` to disable.
    pub scheduled_deletion_sweep_schedule: Option<String>,
}

impl Default for ContentConfig {
    fn default() -> Self {
        Self {
            max_post_length: 5000,
            max_bio_length: 5000,
            max_display_name_length: 100,
            max_profile_fields: 10,
            max_profile_field_name_length: 250,
            max_profile_field_value_length: 2000,
            max_username_length: 100,
            max_registration_message_length: 5000,
            max_report_comment: 2000,
            max_feed_items: 50,
            max_filter_keywords: 100,
            max_filters: 100,
            max_pinned_posts: 5,
            valid_url_schemes: vec!["http".to_string(), "https".to_string()],
            translation_backend: TranslationBackend::GoogleWeb,
            google_translate_api_key: None,
            deepl_api_key: None,
            libretranslate_url: None,
            default_post_content_type: PostContentType::TextPlain,
            link_previews: true,
            link_preview_cache_hours: 24,
            scheduled_status_sweep_schedule: Some("0 * * * *".to_string()),
            scheduled_deletion_sweep_schedule: Some("0 * * * *".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimelinesConfig {
    /// Entries kept per Redis home-feed index. Older entries are
    /// trimmed on fan-out; the SQL read path always holds the full
    /// truth, so this only bounds cache size (Mastodon uses 800).
    pub home_max_entries: u32,
    /// Followers processed per fan-out batch (`ZADD` pipeline).
    pub fanout_batch_size: u32,
    /// Statuses pulled per round-trip when a home feed is rebuilt
    /// from Postgres (cache miss or cold start).
    pub rebuild_page_size: u32,
    /// Whether trending hashtags count remote (federated) statuses in
    /// addition to local ones.
    pub trends_include_remote: bool,
    /// Redis TTL for the trending-tags result cache (seconds).
    pub trends_cache_seconds: u64,
    /// Upper bound on `GET /api/v1/trends/tags?limit=`.
    pub trends_max_limit: u32,
    /// Minimum distinct accounts that must have used a tag for it to
    /// trend (keeps single-user spam off the list).
    pub trends_min_accounts: u32,
}

impl Default for TimelinesConfig {
    fn default() -> Self {
        TimelinesConfig {
            home_max_entries: 800,
            fanout_batch_size: 500,
            rebuild_page_size: 400,
            trends_include_remote: true,
            trends_cache_seconds: 300,
            trends_max_limit: 20,
            trends_min_accounts: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    /// Remove follow relationships in both directions when a user blocks
    /// another user.
    pub unfollow_blocked: bool,
    /// Let a user see the (limited) profile of someone who blocked them.
    pub blockers_visible: bool,
    /// Accounts that newly registered users automatically follow.
    #[serde(deserialize_with = "de::string_or_seq")]
    pub auto_follow_accounts: Vec<String>,
    /// Accounts that automatically follow every newly registered user.
    #[serde(deserialize_with = "de::string_or_seq")]
    pub auto_followed_by: Vec<String>,
    /// Frontend served by default; alternative frontends can be installed
    /// and selected per user.
    pub default_frontend: String,
    /// Days to retain moderation audit log entries.
    pub audit_log_retention_days: u32,
    /// Cron expression for the `audit_log` retention sweep — deletes
    /// entries older than `audit_log_retention_days`. Set to `""` to
    /// disable.
    pub audit_log_sweep_schedule: Option<String>,
    /// Grace period between a self-service account-deletion request and
    /// the federated `Delete` + purge. `0` (Mastodon's behavior) deletes
    /// immediately; during a non-zero grace the account stays
    /// deactivated and an admin can still cancel.
    pub account_deletion_grace_days: u32,
    /// Cron expression for the `account_deletions` sweep that executes
    /// due deletions. Set to `""` to disable.
    pub account_deletion_sweep_schedule: Option<String>,
    /// Cron expression for expired archive-export cleanup. Set to `""`
    /// to disable.
    pub export_cleanup_schedule: Option<String>,
    /// Multipart size cap for `/api/v1/yonder/imports` uploads (bytes).
    pub max_import_size: u64,
    /// Root directory for admin filesystem emoji imports
    /// (`POST /api/v1/yonder/emoji/import_path`). Unset disables the
    /// endpoint — paths are canonicalized and must stay inside it.
    pub emoji_import_dir: Option<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            unfollow_blocked: true,
            blockers_visible: false,
            auto_follow_accounts: Vec::new(),
            auto_followed_by: Vec::new(),
            default_frontend: "yonder".to_string(),
            audit_log_retention_days: 365,
            audit_log_sweep_schedule: Some("0 4 * * *".to_string()),
            account_deletion_grace_days: 0,
            account_deletion_sweep_schedule: Some("*/15 * * * *".to_string()),
            export_cleanup_schedule: Some("0 3 * * *".to_string()),
            max_import_size: 50 * 1024 * 1024,
            emoji_import_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobsConfig {
    /// Run the background worker in-process with `serve`; disable to run
    /// the dedicated `yonder worker` binary instead.
    pub enabled: bool,
    /// Worker tasks draining each queue concurrently.
    pub concurrency: u32,
    /// Delivery attempts before a job is dead-lettered. The default
    /// keeps a failing job retrying for roughly a day — federation
    /// delivery must survive remote deploys and outages.
    pub max_attempts: u32,
    /// Exponential backoff base in seconds (`base ^ attempt`).
    pub backoff_base_seconds: u64,
    /// Cap for the retry backoff in seconds.
    pub backoff_max_seconds: u64,
    /// Maximum execution time per job attempt in seconds; a timed-out
    /// job fails and retries normally. Must stay under
    /// `reclaim_idle_seconds` so a still-running job is never
    /// reclaimed and executed twice.
    pub handler_timeout_seconds: u64,
    /// Pending entries idle this long are reclaimed from dead workers
    /// (`XAUTOCLAIM` threshold).
    pub reclaim_idle_seconds: u64,
    /// Approximate maximum length of each queue's dead-letter stream
    /// (`XTRIM MAXLEN ~`); live queues are never trimmed by length.
    pub stream_maxlen: u64,
    /// Delayed-job poll cadence in milliseconds.
    pub poll_interval_ms: u64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            concurrency: 4,
            max_attempts: 16,
            backoff_base_seconds: 10,
            backoff_max_seconds: 3600,
            handler_timeout_seconds: 240,
            reclaim_idle_seconds: 300,
            stream_maxlen: 10_000,
            poll_interval_ms: 1_000,
        }
    }
}
