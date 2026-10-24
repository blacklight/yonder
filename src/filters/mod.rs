//! Content filters — Mastodon v2 server-side filters (epic 20).
//!
//! `model` owns CRUD over `filters`/`filter_keywords`/`filter_statuses`;
//! `rules` compiles the viewer's active rules for one context; `apply`
//! turns rules into the `hide` SQL clause and the `filtered`
//! attribute. Endpoint layer attaches `filtered` — the endpoint knows
//! the context, `posts::entity` does not.

pub mod apply;
pub mod entity;
pub mod model;
pub mod rules;

pub use model::{FilterRow, KeywordRow};
pub use rules::{FilterContext, Rule};
