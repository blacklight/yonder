//! Reports, instance/domain moderation, audit log — epic 21.
//!
//! - [`domain`]: canonical domain normalization (pubby port).
//! - [`blocks`]: `domain_blocks` queries + enforcement policy reads.
//! - [`audit`]: the single `audit_log` writer + retention job.
//! - [`reports`]: `reports` rows — filing, admin listing, resolution.
//! - [`actions`]: `moderation_actions` — user-level action effects.

pub mod actions;
pub mod audit;
pub mod blocks;
pub mod domain;
pub mod reports;

/// Register this module's job handlers (audit-log retention sweep).
pub fn register_jobs(registry: &mut crate::jobs::Registry) {
    registry.register(
        crate::jobs::JobKind::AuditLogCleanup,
        audit::AuditLogCleanupJob,
    );
}
