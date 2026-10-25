//! E2E coverage for the `yonder` binary itself: the compiled CLI is
//! spawned against the shared test Postgres/Redis under a per-test
//! database name and key prefix. Each test writes an explicit config
//! TOML and clears inherited `YONDER_*` env so nothing leaks in.
//!
//! Skips silently when no container runtime is reachable (same rule
//! as the rest of the suite via `infra()`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use redis::AsyncCommands;
use sqlx::postgres::PgPoolOptions;

use crate::harness::{self, Infra};

/// A tiny valid PNG (1×1, transparent).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// Per-test sandbox: a fresh (unmigrated) database plus a working
/// directory holding the generated config and any fixtures.
struct Sandbox {
    dir: PathBuf,
    config: PathBuf,
    slug: String,
    extra_env: Vec<(String, String)>,
}

async fn sandbox(infra: &Infra, slug: &str) -> Sandbox {
    let db = format!("yonder_t_{slug}");
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

    let dir = std::env::temp_dir().join(format!("yonder-cli-{slug}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    write_config(infra, slug, &db, &dir, &config);
    Sandbox {
        dir,
        config,
        slug: slug.to_string(),
        extra_env: Vec::new(),
    }
}

fn write_config(infra: &Infra, slug: &str, db: &str, dir: &Path, config: &Path) {
    let toml = format!(
        r#"
[server]
migrate_on_start = false
log_level = "warn"

[database]
url = "{pg}/{db}"
max_connections = 3
min_connections = 0

[redis]
url = "{redis}"
key_prefix = "yonder_t:{slug}"

[federation]
enabled = false
allow_private_targets = true

[media]
local_path = "{dir}/media"

[frontends]
dir = "{dir}/frontends"
builtin_dir = "{dir}/builtin"
install_from_path_root = "{dir}/fe-src"

[admin]
emoji_import_dir = "{dir}/emoji-import"
"#,
        pg = infra.pg_url,
        db = db,
        redis = infra.redis_url,
        slug = slug,
        dir = dir.display(),
    );
    std::fs::create_dir_all(dir.join("fe-src")).unwrap();
    std::fs::create_dir_all(dir.join("emoji-import")).unwrap();
    std::fs::write(config, toml).unwrap();
}

/// Spawn the binary. Inherited `YONDER_*` vars are stripped so host
/// env and the repository `.env` cannot override the test config.
fn yonder(sb: &Sandbox, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_yonder"));
    cmd.arg("--config")
        .arg(&sb.config)
        .args(args)
        .current_dir(&sb.dir)
        .stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("YONDER_") && k != "YONDER_CONFIG" {
            cmd.env_remove(&k);
        }
    }
    for (k, v) in &sb.extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn yonder")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).expect("stdout is one JSON value")
}

fn migrate(sb: &Sandbox) {
    let out = yonder(sb, &["migrate"]);
    assert_eq!(code(&out), 0, "migrate failed: {}", stderr(&out));
}

async fn db_pool(sb: &Sandbox) -> sqlx::PgPool {
    let url = format!(
        "{}/yonder_t_{}",
        harness::infra().await.unwrap().pg_url,
        sb.slug
    );
    PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("db pool")
}

async fn create_admin(sb: &Sandbox) {
    let out = yonder(
        sb,
        &[
            "user",
            "create",
            "--username",
            "admin",
            "--email",
            "admin@example.com",
            "--generate-password",
            "--role",
            "admin",
        ],
    );
    assert_eq!(code(&out), 0, "create admin: {}", stderr(&out));
    assert!(stderr(&out).contains("generated password:"));
    assert!(!stdout(&out).contains("generated password:"));
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn migrate_status_three_states() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;

    // Fresh, unmigrated DB: everything pending → exit 1.
    let out = yonder(&sb, &["migrate", "status"]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("pending"));

    migrate(&sb);
    let out = yonder(&sb, &["migrate", "status"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let out = yonder(&sb, &["migrate", "--revert"]);
    assert_eq!(code(&out), 0);
    let out = yonder(&sb, &["migrate", "status", "--json"]);
    assert_eq!(code(&out), 1);
    let rows = stdout_json(&out);
    assert_eq!(
        rows.as_array()
            .unwrap()
            .iter()
            .filter(|r| r["applied"] == false)
            .count(),
        1
    );
}

#[tokio::test]
async fn config_overlay_round_trip() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;

    // Pre-migration write fails with the migrate hint, exit 4.
    let out = yonder(&sb, &["config", "set", "instance.name", "TooEarly"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("migrate"));

    migrate(&sb);

    let out = yonder(&sb, &["config", "set", "instance.name", "Test Inst"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Overlay wins over the env layer.
    let mut env_sb = Sandbox {
        extra_env: vec![("YONDER_INSTANCE__NAME".into(), "env-name".into())],
        ..sandbox_from(&sb)
    };
    let out = yonder(&env_sb, &["config", "get", "instance.name"]);
    assert_eq!(stdout(&out).trim(), "Test Inst");
    env_sb.extra_env.clear();

    // Env layer applies when no overlay exists.
    env_sb
        .extra_env
        .push(("YONDER_INSTANCE__DESCRIPTION".into(), "env-desc".into()));
    let out = yonder(&env_sb, &["config", "get", "instance.description"]);
    assert_eq!(stdout(&out).trim(), "env-desc");

    let out = yonder(&sb, &["config", "unset", "instance.name"]);
    assert_eq!(code(&out), 0);
    // Second unset is a successful no-op.
    let out = yonder(&sb, &["config", "unset", "instance.name"]);
    assert_eq!(code(&out), 0);
    assert!(stderr(&out).contains("no overlay"));

    // Read-only section → 4, nothing written.
    let out = yonder(&sb, &["config", "set", "server.port", "1"]);
    assert_eq!(code(&out), 4);
    // Wrong type → 4, nothing written.
    let out = yonder(
        &sb,
        &["config", "set", "content.max_post_length", "notanumber"],
    );
    assert_eq!(code(&out), 4);
    // Unknown key → 3.
    let out = yonder(&sb, &["config", "get", "nope.nope"]);
    assert_eq!(code(&out), 3);

    // A malformed sensitive overlay must not leak its value into a
    // `config set` error for an unrelated key.
    let pool = db_pool(&sb).await;
    sqlx::query("INSERT INTO config_overlays (key, value) VALUES ('email.smtp_password', '99998888'::jsonb)")
        .execute(&pool)
        .await
        .unwrap();
    let out = yonder(&sb, &["config", "set", "instance.name", "Other"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("99998888"),
        "sensitive overlay value must be redacted: {}",
        stderr(&out)
    );
    // Repair path: overwrite the bad overlay with a valid value.
    let out = yonder(
        &sb,
        &["config", "set", "email.smtp_password", "fixed-password"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

fn sandbox_from(sb: &Sandbox) -> Sandbox {
    Sandbox {
        dir: sb.dir.clone(),
        config: sb.config.clone(),
        slug: sb.slug.clone(),
        extra_env: Vec::new(),
    }
}

#[tokio::test]
async fn config_show_redacts_and_json_errors() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let mut sb = sandbox(infra, &harness::test_slug()).await;
    sb.extra_env
        .push(("YONDER_EMAIL__SMTP_PASSWORD".into(), "s3cr3t-value".into()));
    migrate(&sb);

    let out = yonder(&sb, &["config", "show"]);
    assert!(!stdout(&out).contains("s3cr3t-value"));
    let out = yonder(&sb, &["config", "show", "--reveal"]);
    assert!(stdout(&out).contains("s3cr3t-value"));
    let out = yonder(&sb, &["config", "get", "email.smtp_password"]);
    assert!(stdout(&out).contains("<redacted>"));

    // --json errors: one JSON object on stderr, empty stdout.
    let out = yonder(&sb, &["--json", "config", "get", "nope.nope"]);
    assert_eq!(code(&out), 3);
    assert!(stdout(&out).is_empty());
    let err: serde_json::Value = serde_json::from_str(&stderr(&out)).expect("json error");
    assert_eq!(err["kind"], "not_found");
}

#[tokio::test]
async fn user_lifecycle() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;
    migrate(&sb);

    create_admin(&sb).await;

    // A second regular user for mutation targets.
    let out = yonder(
        &sb,
        &[
            "user",
            "create",
            "--username",
            "bob",
            "--email",
            "bob@example.com",
            "--generate-password",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let out = yonder(&sb, &["user", "list", "--json"]);
    let users = stdout_json(&out);
    let names: Vec<&str> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"admin") && names.contains(&"bob"));

    let out = yonder(&sb, &["--actor", "admin", "user", "verify-email", "bob"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["--actor", "admin", "user", "require-reset", "bob"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Last-admin demotion is refused (exit 4).
    let out = yonder(
        &sb,
        &["--actor", "admin", "user", "set-admin", "admin", "--revoke"],
    );
    assert_eq!(code(&out), 4);

    // Reset URL is a credential: stderr only, never stdout/JSON.
    let out = yonder(&sb, &["--actor", "admin", "user", "reset-password", "bob"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stderr(&out).contains("/auth/password-reset/"));
    assert!(!stdout(&out).contains("password-reset"));

    // Scheduled delete (default) leaves a pending row.
    let out = yonder(&sb, &["--actor", "admin", "user", "delete", "bob", "--yes"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let pool = db_pool(&sb).await;
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletions WHERE state = 'pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 1);

    // Immediate delete purges exactly once.
    let out = yonder(
        &sb,
        &[
            "--actor", "admin", "user", "delete", "bob", "--now", "--yes",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM accounts WHERE username = 'bob'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);

    // --password on argv is refused by clap (exit 2), never echoed.
    let out = yonder(
        &sb,
        &[
            "user",
            "create",
            "--username",
            "x",
            "--email",
            "x@x.co",
            "--password",
            "sekrit",
        ],
    );
    assert_eq!(code(&out), 2);

    // No password source at all is also a usage error (exit 2), not
    // a panic.
    let out = yonder(
        &sb,
        &["user", "create", "--username", "x", "--email", "x@x.co"],
    );
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("password"));
}

#[tokio::test]
async fn domain_block_and_relay() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;
    migrate(&sb);
    create_admin(&sb).await;

    let out = yonder(
        &sb,
        &["--actor", "admin", "domain-block", "add", "bad.example"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["domain-block", "list", "--json"]);
    let blocks = stdout_json(&out);
    assert!(stdout(&out).contains("bad.example"));
    assert_eq!(blocks[0]["severity"], "silence");
    assert!(blocks[0].get("private_comment").is_none());

    // suspend without --yes refuses before mutating (non-TTY → 4).
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "domain-block",
            "add",
            "worse.example",
            "--severity",
            "suspend",
        ],
    );
    assert_eq!(code(&out), 4);
    // Invalid severity → 4.
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "domain-block",
            "add",
            "x.example",
            "--severity",
            "bogus",
        ],
    );
    assert_eq!(code(&out), 4);

    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "domain-block",
            "remove",
            "bad.example",
            "--yes",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["domain-block", "list", "--json"]);
    assert!(!stdout(&out).contains("bad.example"));

    // Unknown block → 3.
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "domain-block",
            "remove",
            "gone.example",
            "--yes",
        ],
    );
    assert_eq!(code(&out), 3);

    // Relays.
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "relay",
            "add",
            "https://relay.example/inbox",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["relay", "list", "--json"]);
    assert!(stdout(&out).contains("relay.example"));
    let relay_id = stdout_json(&out)[0]["id"].as_str().unwrap().to_string();
    let out = yonder(
        &sb,
        &["--actor", "admin", "relay", "remove", &relay_id, "--yes"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Non-https relay URL → 4.
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "relay",
            "add",
            "http://relay.example/inbox",
        ],
    );
    assert_eq!(code(&out), 4);

    // Credential- or token-bearing relay URLs → 4, never echoed.
    for url in [
        "https://relay.example/inbox?token=t0p-secret",
        "https://user:p4ss@relay.example/inbox",
        "https://relay.example/inbox#frag",
    ] {
        let out = yonder(&sb, &["--actor", "admin", "relay", "add", url]);
        assert_eq!(code(&out), 4, "{url}");
        assert!(!stderr(&out).contains("t0p-secret"));
        assert!(!stderr(&out).contains("user:p4ss"));
    }

    // Unknown relay is not-found (3) even without --yes — the
    // existence check precedes any confirmation.
    let out = yonder(&sb, &["relay", "remove", "9999"]);
    assert_eq!(code(&out), 3);
}

#[tokio::test]
async fn run_job_enqueue_inline_and_errors() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;
    migrate(&sb);
    let mut rc = yonder::config::RedisConfig::default();
    let redis = harness::scoped_redis(infra, &mut rc, &sb.slug).await;

    let out = yonder(&sb, &["run-job", "audit_log_cleanup", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let body = stdout_json(&out);
    assert_eq!(body["queue"], "cleanup");
    assert!(body["id"].is_string());
    let key = redis.key("queue:cleanup");
    let len: i64 = redis.conn().await.unwrap().xlen(&key).await.expect("xlen");
    assert!(len >= 1, "enqueued job lands on the cleanup stream");

    let out = yonder(&sb, &["run-job", "audit_log_cleanup", "--inline"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    let out = yonder(&sb, &["run-job", "nope"]);
    assert_eq!(code(&out), 3);
    let out = yonder(
        &sb,
        &["run-job", "audit_log_cleanup", "--payload", "not json"],
    );
    assert_eq!(code(&out), 4);
    let out = yonder(
        &sb,
        &["run-job", "audit_log_cleanup", "--inline", "--delay", "5"],
    );
    assert_eq!(code(&out), 4);

    // --delay enqueues into the delayed ZSET at ~now + delay.
    let before_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let out = yonder(&sb, &["run-job", "audit_log_cleanup", "--delay", "60"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let after_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;
    let delayed_key = redis.key("jobs:delayed");
    let scored: Vec<(String, f64)> = redis
        .conn()
        .await
        .unwrap()
        .zrange_withscores(&delayed_key, 0, -1)
        .await
        .expect("zrange delayed");
    assert_eq!(scored.len(), 1, "delayed job parked in ZSET");
    let score = scored[0].1;
    assert!(
        score >= before_ms + 60_000.0 && score <= after_ms + 60_000.0,
        "delayed run-at {score} should be ≈ now + 60s, got window [{}, {}]",
        before_ms + 60_000.0,
        after_ms + 60_000.0
    );
}

#[tokio::test]
async fn emoji_and_frontend() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;
    migrate(&sb);
    create_admin(&sb).await;

    // Single file import.
    let file = sb.dir.join("blobcat.png");
    std::fs::write(&file, PNG).unwrap();
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "emoji",
            "import",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Directory import (inside admin.emoji_import_dir).
    let pack_dir = sb.dir.join("emoji-import/pack1");
    std::fs::create_dir_all(&pack_dir).unwrap();
    std::fs::write(pack_dir.join("party.png"), PNG).unwrap();
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "emoji",
            "import",
            pack_dir.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("party"));

    // Zip import.
    let zip_path = sb.dir.join("emojis.zip");
    {
        let f = std::fs::File::create(&zip_path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        w.start_file::<_, ()>("zipcat.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        use std::io::Write;
        w.write_all(PNG).unwrap();
        w.finish().unwrap();
    }
    let out = yonder(
        &sb,
        &[
            "--actor",
            "admin",
            "emoji",
            "import",
            zip_path.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Frontend install from a directory inside install_from_path_root.
    let fe_src = sb.dir.join("fe-src/myfe");
    std::fs::create_dir_all(&fe_src).unwrap();
    std::fs::write(fe_src.join("index.html"), "<html>ok</html>").unwrap();
    let out = yonder(&sb, &["frontend", "install", fe_src.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["frontend", "list", "--json"]);
    assert!(stdout(&out).contains("myfe"));
    // Non-TTY remove without --yes refuses (exit 4) before deleting.
    let out = yonder(&sb, &["frontend", "remove", "myfe"]);
    assert_eq!(code(&out), 4);
    let out = yonder(&sb, &["frontend", "remove", "myfe", "--yes"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));

    // Unknown frontend is not-found (3) even without --yes — the
    // existence check precedes any confirmation.
    let out = yonder(&sb, &["frontend", "remove", "ghost"]);
    assert_eq!(code(&out), 3);

    // A credential-bearing git URL is rejected before it can reach a
    // clone command line or an error message — and is never echoed.
    let out = yonder(
        &sb,
        &[
            "frontend",
            "install",
            "https://u:t0p-secret@example.com/repo.git",
        ],
    );
    assert_eq!(code(&out), 4);
    assert!(
        !stderr(&out).contains("t0p-secret"),
        "credentials must not appear in stderr: {}",
        stderr(&out)
    );
    // An ssh username is fine; only passwords are credentials. git
    // installs are disabled in the test config, so the failure is the
    // disabled error — not a credential rejection.
    let out = yonder(
        &sb,
        &["frontend", "install", "ssh://git@example.com/repo.git"],
    );
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("disabled"), "{}", stderr(&out));
}

#[tokio::test]
async fn instance_sugar_and_help() {
    let Some(infra) = harness::infra().await else {
        return;
    };
    let _permit = harness::acquire().await;
    let sb = sandbox(infra, &harness::test_slug()).await;
    migrate(&sb);

    let out = yonder(&sb, &["instance", "set-name", "My Instance"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["config", "get", "instance.name"]);
    assert_eq!(stdout(&out).trim(), "My Instance");

    let out = yonder(&sb, &["instance", "set-domain", "example.com"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = yonder(&sb, &["config", "get", "instance.domain"]);
    assert_eq!(stdout(&out).trim(), "example.com");
    let out = yonder(&sb, &["instance", "set-domain", "https://bad.example/"]);
    assert_eq!(code(&out), 4);

    // --help works at every level tested.
    for args in [
        vec!["--help"],
        vec!["user", "--help"],
        vec!["user", "create", "--help"],
        vec!["domain-block", "--help"],
        vec!["run-job", "--help"],
        vec!["migrate", "status", "--help"],
    ] {
        let out = yonder(&sb, &args);
        assert_eq!(code(&out), 0, "--help failed for {args:?}");
    }
}
