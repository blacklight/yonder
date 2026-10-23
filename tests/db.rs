//! Database integration tests: migrations, schema invariants and the
//! `config_overlays` contract, run against a real Postgres in a
//! testcontainer.
//!
//! Skipped (with a message, never silently green) when no Docker-compatible
//! container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::path::Path;

use sqlx::{PgPool, Row};
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::postgres::Postgres;
use yonder::config::Config;
use yonder::db;

/// Heuristic check for a reachable container runtime: either `DOCKER_HOST`
/// is set or the default socket exists.
fn docker_available() -> bool {
    std::env::var_os("DOCKER_HOST").is_some() || Path::new("/var/run/docker.sock").exists()
}

/// Start a fresh Postgres container and return (pool, container). Each test
/// gets its own container, so tests are fully isolated.
async fn test_pool() -> Option<(PgPool, testcontainers::ContainerAsync<Postgres>)> {
    if !docker_available() {
        eprintln!("skipping: no container runtime reachable (set DOCKER_HOST or expose /var/run/docker.sock)");
        return None;
    }
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("failed to start postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("failed to resolve container port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPool::connect(&url).await.expect("failed to connect");
    Some((pool, container))
}

#[tokio::test]
async fn migrations_apply_idempotently() {
    let Some((pool, container)) = test_pool().await else {
        return;
    };

    db::migrate::run(&pool).await.unwrap();
    let after_first = db::migrate::applied_versions(&pool).await.unwrap();
    assert_eq!(after_first, (1..=16).collect::<Vec<i64>>());

    db::migrate::run(&pool).await.unwrap();
    let after_second = db::migrate::applied_versions(&pool).await.unwrap();
    assert_eq!(after_first, after_second);

    container.stop().await.unwrap();
}

#[tokio::test]
async fn down_migrations_revert_cleanly() {
    let Some((pool, container)) = test_pool().await else {
        return;
    };

    db::migrate::run(&pool).await.unwrap();

    // Single-step revert: 16 applied -> 15 applied.
    db::migrate::revert(&pool, None).await.unwrap();
    let versions = db::migrate::applied_versions(&pool).await.unwrap();
    assert_eq!(versions, (1..=15).collect::<Vec<i64>>());

    // Full revert: only the migrations bookkeeping table remains.
    db::migrate::revert(&pool, Some(0)).await.unwrap();
    let tables: Vec<String> = sqlx::query(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<String, _>("tablename"))
    .collect();
    assert_eq!(tables, vec!["_sqlx_migrations".to_string()]);

    // And the schema can be rebuilt from scratch.
    db::migrate::run(&pool).await.unwrap();
    assert_eq!(
        db::migrate::applied_versions(&pool).await.unwrap(),
        (1..=16).collect::<Vec<i64>>()
    );

    container.stop().await.unwrap();
}

/// The PROMPT's acceptance smoke test: a fresh schema accepts a user, a
/// status, a follow, a like and a notification with referential integrity.
#[tokio::test]
async fn smoke_test_referential_integrity() {
    let Some((pool, container)) = test_pool().await else {
        return;
    };
    db::migrate::run(&pool).await.unwrap();

    let alice: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (username, actor_id, url)
         VALUES ('alice', 'https://example.com/users/alice', 'https://example.com/@alice')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let bob: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (username, actor_id, url)
         VALUES ('bob', 'https://example.com/users/bob', 'https://example.com/@bob')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let alice_user: i64 = sqlx::query_scalar(
        "INSERT INTO users (account_id, email, password_hash)
         VALUES ($1, 'alice@example.com', 'argon2id$fake')
         RETURNING id",
    )
    .bind(alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(alice_user > 0);

    let status: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (account_id, content, uri)
         VALUES ($1, 'hello world', 'https://example.com/users/alice/statuses/1')
         RETURNING id",
    )
    .bind(alice)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $2)")
        .bind(bob)
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("INSERT INTO favourites (account_id, status_id) VALUES ($1, $2)")
        .bind(bob)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO notifications (account_id, from_account_id, type, status_id)
         VALUES ($1, $2, 'favourite', $3)",
    )
    .bind(alice)
    .bind(bob)
    .bind(status)
    .execute(&pool)
    .await
    .unwrap();

    // Referential integrity: a notification for a nonexistent status fails.
    let missing: sqlx::Error = sqlx::query(
        "INSERT INTO notifications (account_id, from_account_id, type, status_id)
         VALUES ($1, $2, 'favourite', $3)",
    )
    .bind(alice)
    .bind(bob)
    .bind(999_999_i64)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(matches!(missing, sqlx::Error::Database(_)));

    container.stop().await.unwrap();
}

/// `config_overlays` rows flow through the epic-01 read path into the
/// effective configuration.
#[tokio::test]
async fn config_overlays_round_trip() {
    let Some((pool, container)) = test_pool().await else {
        return;
    };
    db::migrate::run(&pool).await.unwrap();

    sqlx::query("INSERT INTO config_overlays (key, value) VALUES ($1, $2::jsonb)")
        .bind("instance.name")
        .bind(serde_json::json!("Overridden"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO config_overlays (key, value) VALUES ($1, $2::jsonb)")
        .bind("server.port")
        .bind(serde_json::json!(4567))
        .execute(&pool)
        .await
        .unwrap();

    let overlays = db::config_overlays::fetch(&pool).await.unwrap();
    assert_eq!(overlays.len(), 2);

    let config = Config::with_overlays(None, &overlays).unwrap();
    assert_eq!(config.instance.name, "Overridden");
    assert_eq!(config.server.port, 4567);

    container.stop().await.unwrap();
}

#[tokio::test]
async fn check_constraints_enforced() {
    let Some((pool, container)) = test_pool().await else {
        return;
    };
    db::migrate::run(&pool).await.unwrap();

    let alice: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (username, actor_id) VALUES ('alice', 'https://example.com/users/alice')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    for (sql, bind_status) in [
        (
            "INSERT INTO statuses (account_id, visibility) VALUES ($1, 'bogus')",
            false,
        ),
        (
            "INSERT INTO statuses (account_id, content_type) VALUES ($1, 'text/html')",
            false,
        ),
        (
            "INSERT INTO statuses (account_id, content) VALUES ($1, repeat('x', 20001))",
            false,
        ),
    ] {
        let mut query = sqlx::query(sql).bind(alice);
        if bind_status {
            query = query.bind(true);
        }
        let error = query.execute(&pool).await.unwrap_err();
        assert!(
            matches!(error, sqlx::Error::Database(_)),
            "expected CHECK violation for {sql}"
        );
    }

    // Self-follow is rejected.
    let error = sqlx::query("INSERT INTO follows (account_id, target_account_id) VALUES ($1, $1)")
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(matches!(error, sqlx::Error::Database(_)));

    container.stop().await.unwrap();
}
