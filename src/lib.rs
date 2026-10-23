//! Yonder — a Mastodon-compatible ActivityPub server in Rust.
//!
//! This crate is the single `yonder` binary's library half: every epic adds
//! its domain code as a module here while `main.rs` stays a thin shell over
//! [`cli::main`].

pub mod activitypub;
pub mod admin;
pub mod auth;
pub mod cache;
pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod feeds;
pub mod http;
pub mod i18n;
pub mod interactions;
pub mod jobs;
pub mod logging;
pub mod media;
pub mod moderation;
pub mod posts;
pub mod search;
pub mod users;

pub use error::{Error, Result};
