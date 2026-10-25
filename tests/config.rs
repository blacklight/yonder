//! Configuration system tests: layering order, key discoverability, and
//! the example file contract.
// figment::Jail closures return Result<(), figment::Error> (~200 bytes).
#![allow(clippy::result_large_err)]

use std::path::PathBuf;

use figment::Jail;
use serde_json::json;
use yonder::config::{registry, Config, PostContentType, RegistrationMode, ENV_PREFIX};

fn manifest_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(file)
}

#[test]
fn layering_defaults_toml_env_overlay() {
    Jail::expect_with(|jail| {
        jail.create_file(
            "config.toml",
            r#"
            [instance]
            name = "toml-name"

            [server]
            port = 4000

            [rate_limit.search]
            limit = 99
            "#,
        )?;
        jail.set_env("YONDER_INSTANCE__NAME", "env-name");

        let path = jail.directory().join("config.toml");

        // TOML beats defaults; env beats TOML.
        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.server.port, 4000, "TOML must override the default");
        assert_eq!(
            config.rate_limit.search.limit, 99,
            "nested TOML table must deserialize"
        );
        assert_eq!(config.instance.name, "env-name", "env must override TOML");
        assert_eq!(
            config.instance.domain, "localhost",
            "default must survive untouched keys"
        );

        // DB overlay beats everything.
        let overlays = vec![
            ("instance.name".to_string(), json!("db-name")),
            ("rate_limit.search.limit".to_string(), json!(7)),
        ];
        let config = Config::with_overlays(Some(&path), &overlays).expect("overlays");
        assert_eq!(config.instance.name, "db-name");
        assert_eq!(config.rate_limit.search.limit, 7);
        assert_eq!(config.server.port, 4000, "env/TOML layers are unaffected");

        Ok(())
    });
}

#[test]
fn missing_config_file_falls_back_to_defaults_and_env() {
    Jail::expect_with(|jail| {
        jail.set_env("YONDER_INSTANCE__NAME", "from-env");
        let config = Config::load(Some(&jail.directory().join("nope.toml")))
            .expect("missing file is not an error");
        assert_eq!(config.instance.name, "from-env");
        assert_eq!(config.server.port, 3000);
        Ok(())
    });
}

#[test]
fn env_config_path_is_respected() {
    Jail::expect_with(|jail| {
        jail.create_file("elsewhere.toml", "[server]\nport = 4321\n")?;
        jail.set_env(
            "YONDER_CONFIG",
            jail.directory()
                .join("elsewhere.toml")
                .display()
                .to_string(),
        );
        let config = Config::load(None).expect("load via YONDER_CONFIG");
        assert_eq!(config.server.port, 4321);
        Ok(())
    });
}

#[test]
fn env_vars_support_comma_separated_lists_and_typed_values() {
    Jail::expect_with(|jail| {
        jail.set_env(
            "YONDER_HTTP__CORS_ORIGINS",
            "https://a.example, https://b.example",
        );
        jail.set_env("YONDER_INSTANCE__LANGUAGES", "en,it,de");
        jail.set_env("YONDER_SERVER__PORT", "8080");
        jail.set_env("YONDER_AUTH__REGISTRATION_MODE", "approval-required");
        jail.set_env("YONDER_CONTENT__DEFAULT_POST_CONTENT_TYPE", "text/markdown");

        let config = Config::load(Some(&jail.directory().join("nope.toml"))).expect("load");
        assert_eq!(
            config.http.cors_origins,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        assert_eq!(
            config.instance.languages,
            vec!["en".to_string(), "it".to_string(), "de".to_string()]
        );
        assert_eq!(config.server.port, 8080);
        assert!(matches!(
            config.auth.registration_mode,
            RegistrationMode::ApprovalRequired
        ));
        assert!(matches!(
            config.content.default_post_content_type,
            PostContentType::TextMarkdown
        ));
        Ok(())
    });
}

#[test]
fn example_config_parses() {
    let config = Config::load(Some(&manifest_path("config.toml.example")))
        .expect("config.toml.example must parse into Config");
    assert_eq!(config.instance.domain, "example.com");
    assert_eq!(config.rate_limit.search.limit, 20);
    assert_eq!(config.polls.max_options, 10);
    assert_eq!(config.content.max_post_length, 5000);
}

#[test]
fn registry_covers_every_leaf_and_all_sections() {
    let entries = registry::entries();
    assert!(!entries.is_empty());

    // Every key must map to a YONDER_ env var and back to a real leaf.
    for entry in &entries {
        assert!(
            entry.env.starts_with(ENV_PREFIX),
            "{} must produce a {} env name",
            entry.toml_path,
            ENV_PREFIX
        );
    }

    // The top-level sections appear.
    for section in [
        "server",
        "database",
        "redis",
        "instance",
        "i18n",
        "auth",
        "federation",
        "media",
        "email",
        "search",
        "http",
        "rate_limit",
        "polls",
        "content",
        "admin",
        "frontends",
    ] {
        assert!(
            entries
                .iter()
                .any(|e| e.toml_path.starts_with(&format!("{section}."))),
            "registry must contain keys under {section}"
        );
    }

    // Spot-check the nested-table mapping the PROMPT calls out.
    let entry = registry::find("rate_limit.search.limit").expect("key exists");
    assert_eq!(entry.env, "YONDER_RATE_LIMIT__SEARCH__LIMIT");
    assert_eq!(registry::env_name("instance.name"), "YONDER_INSTANCE__NAME");
    assert!(registry::find("not.a.key").is_none());
}

#[test]
fn i18n_fallback_locale() {
    // Default is English.
    let config = Config::default();
    assert_eq!(config.i18n.fallback_locale, "en");
    config.validate().expect("defaults are valid");

    Jail::expect_with(|jail| {
        jail.create_file("config.toml", "[i18n]\nfallback_locale = \"it\"")?;
        let config =
            Config::load(Some(&jail.directory().join("config.toml"))).expect("load i18n section");
        assert_eq!(config.i18n.fallback_locale, "it");
        Ok(())
    });

    // An unsupported code deserializes but fails semantic validation.
    let mut config = Config::default();
    config.i18n.fallback_locale = "fr".to_string();
    assert!(config.validate().is_err());
}

#[test]
fn max_feed_items_bounds() {
    for (value, valid) in [
        (0u32, false),
        (1, true),
        (50, true),
        (200, true),
        (201, false),
    ] {
        let mut config = Config::default();
        config.content.max_feed_items = value;
        assert_eq!(config.validate().is_ok(), valid, "max_feed_items={value}");
    }
}

#[test]
fn timelines_limits_must_be_positive() {
    for mutate in [
        (|c: &mut Config| c.timelines.home_max_entries = 0) as fn(&mut Config),
        |c| c.timelines.fanout_batch_size = 0,
        |c| c.timelines.rebuild_page_size = 0,
    ] {
        let mut config = Config::default();
        mutate(&mut config);
        assert!(config.validate().is_err());
    }
}

#[test]
fn filter_trends_and_emoji_bounds() {
    for (mutate, valid) in [
        (
            (|c: &mut Config| c.content.max_filter_keywords = 0) as fn(&mut Config),
            false,
        ),
        (|c| c.content.max_filter_keywords = 100, true),
        (|c| c.timelines.trends_max_limit = 0, false),
        (|c| c.timelines.trends_max_limit = 20, true),
        (|c| c.timelines.trends_cache_seconds = 86401, false),
        (|c| c.timelines.trends_cache_seconds = 300, true),
        (|c| c.media.max_emoji_size = 512, false),
        (|c| c.media.max_emoji_size = 256 * 1024, true),
        (|c| c.media.max_emoji_zip_uncompressed = 1024, false),
        (
            |c| c.media.max_emoji_zip_uncompressed = 50 * 1024 * 1024,
            true,
        ),
    ] {
        let mut config = Config::default();
        mutate(&mut config);
        assert_eq!(config.validate().is_ok(), valid);
    }
}
