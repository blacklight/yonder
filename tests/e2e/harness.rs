//! Shared test infrastructure: one Postgres + one Redis container per
//! run, per-test isolation via database/key-prefix namespacing.
//!
//! Every test used to spawn its own container pair — with ~250 tests
//! that dominated suite runtime. Now:
//!
//! - `infra()` lazily starts **named** `yonder-test-pg17` /
//!   `yonder-test-redis7` containers with `ReuseDirective::Always`, so
//!   they are created once and re-attached on every subsequent `cargo
//!   test` run (delete them with `docker rm -f` to start clean).
//! - `provision_db()` gives each test a fresh `yonder_t_<test>`
//!   database — deterministic name, `DROP`+`CREATE` wipes leftovers.
//! - `scoped_redis()` points the test at the shared Redis under a
//!   per-test `key_prefix` — Yonder already derives every key, stream,
//!   queue and pub/sub channel (including Pubby's document store) from
//!   `redis.key_prefix`, so tests cannot cross-talk.
//!
//! CI can skip testcontainers entirely by exporting
//! `YONDER_TEST_PG_URL` (admin-capable base URL, no database path, e.g.
//! `postgres://postgres:postgres@localhost:5432`) and/or
//! `YONDER_TEST_REDIS_URL` (e.g. `redis://localhost:6379`).
//!
//! The test slug is the libtest thread name — the full test path — so
//! names are deterministic and unique per test.

use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use redis::AsyncCommands;
use sqlx::{postgres::PgPoolOptions, PgPool};
use testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt, ReuseDirective};
use testcontainers_modules::{postgres::Postgres, redis::Redis as RedisImage};
use tokio::sync::OnceCell;
use yonder::{cache, config::RedisConfig, db};

/// Is docker (or a compatible runtime) reachable?
pub fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

/// Everything a test needs to reach the shared services.
///
/// Deliberately holds only plain data + container handles: each
/// `#[tokio::test]` runs on its own runtime that shuts down with the
/// test, so a cached `PgPool`/bb8 pool stashed here would die with the
/// first test's runtime. Connection-bound handles are created per call
/// in the caller's runtime instead.
pub struct Infra {
    /// Admin-capable Postgres base URL (no database path).
    pub pg_url: String,
    /// Redis base URL (no key prefix — that is per test).
    pub redis_url: String,
    // Keep the handles alive so reused containers stay attributed to
    // this process; with `Always` they survive the process anyway.
    _pg: Option<ContainerAsync<Postgres>>,
    _redis: Option<ContainerAsync<RedisImage>>,
}

static INFRA: OnceCell<Option<Infra>> = OnceCell::const_new();

/// Held for the duration of a test — bounds how many tests run
/// concurrently. Returned by [`acquire`]; dropping it frees a slot.
pub type Permit = tokio::sync::OwnedSemaphorePermit;

static PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn semaphore() -> &'static Arc<tokio::sync::Semaphore> {
    PERMITS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(max_workers())))
}

/// Concurrent-test budget: 3/4 of available cores (leaving headroom on
/// shared boxes) — override with `YONDER_TEST_WORKERS`. libtest still
/// spawns a thread per core; the semaphore parks surplus tests at
/// their first `acquire` instead of letting them all run at once.
fn max_workers() -> usize {
    if let Some(v) = std::env::var("YONDER_TEST_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return v.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| (n.get() * 3 / 4).max(1))
        .unwrap_or(1)
}

/// Take one of the [`max_workers`] concurrent-test slots. Container-
/// backed tests should call this first (the env helpers' callers do it
/// via `let _permit = harness::acquire().await;`).
pub async fn acquire() -> Permit {
    semaphore()
        .clone()
        .acquire_owned()
        .await
        .expect("permit semaphore closed")
}

/// Lazily start (or attach to) the shared containers. `None` when no
/// container runtime is reachable and no `YONDER_TEST_*_URL` overrides
/// are set — callers treat that as "skip".
pub async fn infra() -> Option<&'static Infra> {
    INFRA
        .get_or_init(|| async {
            let pg_url_override = std::env::var("YONDER_TEST_PG_URL").ok();
            let redis_url_override = std::env::var("YONDER_TEST_REDIS_URL").ok();

            // Any service without a URL override needs a container.
            if (pg_url_override.is_none() || redis_url_override.is_none()) && !docker_available() {
                eprintln!(
                    "skipping: no container runtime reachable \
                     (set DOCKER_HOST or expose /var/run/docker.sock)"
                );
                return None;
            }

            let mut pg = None;
            let mut redis = None;
            let pg_url = match pg_url_override {
                Some(url) => url,
                None => {
                    // The spec string feeds the reuse fingerprint —
                    // bump it whenever the image options below change.
                    let c = start_reused("yonder-test-pg17", "pg17+c400+nofsync+v1", || {
                        Postgres::default()
                            .with_tag("17-alpine")
                            // Parallel tests each hold a small pool; keep
                            // headroom so ~250 tests can't exhaust the
                            // default 100-connection cap. Durability is
                            // irrelevant in tests — skip the fsync cost.
                            .with_cmd([
                                "postgres".to_string(),
                                "-c".to_string(),
                                "max_connections=400".to_string(),
                                "-c".to_string(),
                                "fsync=off".to_string(),
                                "-c".to_string(),
                                "synchronous_commit=off".to_string(),
                                "-c".to_string(),
                                "full_page_writes=off".to_string(),
                            ])
                    })
                    .await;
                    let port = c.get_host_port_ipv4(5432).await.expect("postgres port");
                    pg = Some(c);
                    format!("postgres://postgres:postgres@127.0.0.1:{port}")
                }
            };
            let redis_url = match redis_url_override {
                Some(url) => url,
                None => {
                    let c = start_reused("yonder-test-redis7", "redis7", || {
                        RedisImage::default().with_tag("7-alpine")
                    })
                    .await;
                    let port = c.get_host_port_ipv4(6379).await.expect("redis port");
                    redis = Some(c);
                    format!("redis://127.0.0.1:{port}")
                }
            };

            // Sanity: reach the services before tests start.
            PgPoolOptions::new()
                .max_connections(1)
                .connect(&format!("{pg_url}/postgres"))
                .await
                .expect("postgres admin connection");

            Some(Infra {
                pg_url,
                redis_url,
                _pg: pg,
                _redis: redis,
            })
        })
        .await
        .as_ref()
}

/// Start `name` as a reusable container: attach if a running container
/// matches (same name + config-hash label), create otherwise. Retries
/// around the create/attach race (two test processes cold-starting at
/// once) and stale stopped or outdated-config containers.
async fn start_reused<I, F>(name: &str, config: &str, image: F) -> ContainerAsync<I>
where
    I: testcontainers::Image,
    F: Fn() -> testcontainers::ContainerRequest<I>,
{
    // Config hash in the label: a container created with different
    // flags/env doesn't match the reuse lookup and gets recreated.
    let fingerprint = format!("{name}:{}", hash(config));
    for attempt in 0..5 {
        let req = image()
            .with_container_name(name)
            .with_label("org.yonder.test-infra", &fingerprint)
            .with_reuse(ReuseDirective::Always);
        match req.start().await {
            Ok(c) => return c,
            Err(e) if attempt < 4 => {
                // Name conflicts: a stopped or stale-config container
                // is removed; a running one with our fingerprint means
                // a concurrent process won the race and the next
                // attempt attaches via reuse.
                let state = inspect(name);
                if state.as_deref() != Some("true") || state_label(name) != fingerprint {
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "-f", name])
                        .output();
                }
                eprintln!("retrying {name} start after {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("{name} failed to start: {e}"),
        }
    }
    unreachable!()
}

/// `docker inspect` helper: the container's `State.Running`, or `None`
/// when it doesn't exist.
fn inspect(name: &str) -> Option<String> {
    std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn state_label(name: &str) -> String {
    std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{index .Config.Labels \"org.yonder.test-infra\"}}",
            name,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Unique, deterministic per-test name derived from the libtest
/// thread name (the full test path), e.g. `admin::settings_reset` →
/// `admin_settings_reset`. Truncated to fit Postgres' 63-byte ident.
pub fn test_slug() -> String {
    let name = std::thread::current()
        .name()
        .unwrap_or("anonymous")
        .to_string();
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    slug.chars().take(48).collect()
}

/// Fresh database `yonder_t_<slug>` on the shared Postgres, migrated
/// and ready. Returns a small pool — tests run in parallel and share
/// the container's connection budget.
pub async fn provision_db(infra: &Infra, slug: &str) -> PgPool {
    let db = format!("yonder_t_{slug}");
    // A fresh maintenance connection per call — see `Infra`'s note on
    // runtime-bound pools. DROP+CREATE cannot run in a transaction.
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}/postgres", infra.pg_url))
        .await
        .expect("postgres admin connection");
    sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop test database");
    sqlx::query(&format!("CREATE DATABASE \"{db}\""))
        .execute(&admin)
        .await
        .expect("create test database");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&format!("{}/{db}", infra.pg_url))
        .await
        .expect("connect test database");
    db::migrate::run(&pool).await.expect("migrations");
    pool
}

/// Point `config.redis` at the shared Redis under a per-test
/// `key_prefix`, evict any keys a previous run left under that prefix,
/// and return the connected handle (the caller still passes
/// `&config.redis` to `cache::connect`-equivalent paths via the
/// returned handle).
pub async fn scoped_redis(infra: &Infra, config: &mut RedisConfig, slug: &str) -> cache::Redis {
    config.url = infra.redis_url.clone();
    config.key_prefix = format!("yonder_t:{slug}");
    let redis = cache::connect(config).await.expect("redis connect");

    // Reused containers keep keys across runs — evict this test's
    // namespace so assertions on empty queues/caches see a clean slate.
    let pattern = format!("{}:*", redis.prefix());
    let mut stale = Vec::new();
    {
        let mut conn = redis.conn().await.expect("redis conn");
        let mut cursor = conn
            .scan_match::<_, String>(pattern)
            .await
            .expect("scan test prefix");
        while let Some(key) = cursor.next_item().await {
            stale.push(key);
        }
    }
    if !stale.is_empty() {
        let mut conn = redis.conn().await.expect("redis conn");
        for chunk in stale.chunks(500) {
            let _: () = conn.del(chunk).await.expect("del stale keys");
        }
    }
    redis
}

/// A dedicated (NOT shared) Redis container for tests that control the
/// service lifecycle — e.g. dropping the handle to simulate an outage.
/// Everything else should use [`scoped_redis`].
pub async fn dedicated_redis(
    config: &mut RedisConfig,
    slug: &str,
) -> Option<(cache::Redis, ContainerAsync<RedisImage>)> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable");
        return None;
    }
    let container = RedisImage::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    config.url = format!("redis://127.0.0.1:{port}");
    config.key_prefix = format!("yonder_t:{slug}");
    let redis = cache::connect(config).await.expect("redis connect");
    Some((redis, container))
}
