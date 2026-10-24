//! End-to-end integration tests — every container-backed suite merged
//! into one target so libtest runs them all in parallel (default
//! `--test-threads` = available cores) against a single shared
//! Postgres+Redis pair (`harness` namespaces each test by database and
//! key prefix).
//!
//! `tests/config.rs` stays a separate target: `figment::Jail` mutates
//! process env vars, which must not leak into the shared-infra tests.

mod harness;

mod account_management;
mod activitypub;
mod admin;
mod auth;
mod db;
mod feeds;
mod filters_tags_emoji;
mod http;
mod interactions;
mod media;
mod moderation;
mod notifications;
mod posting;
mod profile;
mod redis;
mod search;
mod settings;
mod social_graph;
mod timelines;
mod two_factor;
