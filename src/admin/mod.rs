//! Instance administration (epic 22): settings overlays, the
//! audit-log reader, relays, email transport and WebPush keys.
//!
//! - [`settings`]: describe/set/reset `config_overlays` for the admin
//!   panel, with read-only/secret/restart policies.
//! - [`audit`]: `GET /api/v1/admin/audit_log` reader over epic 21's
//!   writer (`crate::moderation::audit`).
//! - [`relays`]: relay CRUD + manual authorization; fan-out itself
//!   lives in `activitypub::delivery`.
//! - [`email`]: SMTP transport (`lettre`), editable templates and the
//!   real `EmailSend` handler (replaces the epic-03 stub).
//! - [`webpush`]: VAPID keypair generation stored as overlays.

pub mod audit;
pub mod email;
pub mod relays;
pub mod settings;
pub mod webpush;

/// Register this module's job handlers — the real `EmailSend` sender.
/// Must run after `jobs::stub::register_all` so it overwrites the stub.
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    email::register_jobs(registry);
}
