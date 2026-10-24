//! Mastodon OAuth scope model.
//!
//! Base scopes (`read`, `write`, `follow`, `push`, `admin`) imply their
//! granular forms through the `:` hierarchy: a token scope `T` covers a
//! required scope `R` when `R == T` or `R` starts with `T:` — so
//! `write` covers `write:statuses` and `admin:write` covers
//! `admin:write:reports`.
//!
//! Scope *coverage* is only half the check: [`require_scope`] also
//! applies the account-state gate — non-`read` scopes additionally
//! demand an approved account and a verified email (when verification
//! is required) — and `admin` demands the `admin` role.

use std::collections::BTreeSet;

use crate::{config::Config, http::middleware::auth::AuthContext, Error, Result};

/// Every scope string clients may request — the standard Mastodon set.
const KNOWN_SCOPES: &[&str] = &[
    "read",
    "write",
    "follow",
    "push",
    "admin",
    "profile",
    // read:*
    "read:accounts",
    "read:blocks",
    "read:bookmarks",
    "read:favourites",
    "read:filters",
    "read:follows",
    "read:lists",
    "read:mutes",
    "read:notifications",
    "read:search",
    "read:statuses",
    // write:*
    "write:accounts",
    "write:blocks",
    "write:bookmarks",
    "write:conversations",
    "write:favourites",
    "write:filters",
    "write:follows",
    "write:lists",
    "write:media",
    "write:mutes",
    "write:notifications",
    "write:reports",
    "write:statuses",
    // admin:read*
    "admin:read",
    "admin:read:accounts",
    "admin:read:reports",
    "admin:read:domain_allows",
    "admin:read:domain_blocks",
    "admin:read:ip_blocks",
    "admin:read:email_domain_blocks",
    "admin:read:canonical_email_blocks",
    // admin:write*
    "admin:write",
    "admin:write:accounts",
    "admin:write:reports",
    "admin:write:domain_allows",
    "admin:write:domain_blocks",
    "admin:write:ip_blocks",
    "admin:write:email_domain_blocks",
    "admin:write:canonical_email_blocks",
];

/// Parsed, validated scope set (space-separated on the wire).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSet(pub BTreeSet<String>);

impl ScopeSet {
    /// Parse a space-separated scope string; unknown scopes are rejected.
    pub fn parse(scopes: &str) -> Result<ScopeSet> {
        let mut out = BTreeSet::new();
        for scope in scopes.split_whitespace() {
            if !KNOWN_SCOPES.contains(&scope) {
                return Err(Error::Validation(format!("unknown scope '{scope}'")));
            }
            out.insert(scope.to_string());
        }
        Ok(ScopeSet(out))
    }

    /// The canonical space-separated form.
    pub fn as_string(&self) -> String {
        self.0.iter().cloned().collect::<Vec<_>>().join(" ")
    }

    /// Whether every scope in `self` is covered by `parent`'s scopes.
    pub fn is_subset_of(&self, parent: &ScopeSet) -> bool {
        self.0.iter().all(|s| parent.covers(s))
    }

    /// Whether `required` is covered by any scope in this set.
    pub fn covers(&self, required: &str) -> bool {
        self.0.iter().any(|s| {
            required == s
                || required
                    .strip_prefix(s.as_str())
                    .is_some_and(|rest| rest.starts_with(':'))
        })
    }
}

/// Whether a scope lives under the `read` umbrella (read scopes escape
/// the approval/verification gate — anonymous-ish viewing rights).
fn is_read_scope(scope: &str) -> bool {
    scope == "read" || scope.starts_with("read:")
}

/// Whether the configured instance requires email verification before
/// posting (needs both the flag and a mail server to send links with).
pub fn requires_email_verification(config: &Config) -> bool {
    config.auth.require_email_verification && config.email.smtp_host.is_some()
}

/// Enforce that `auth`'s token covers `required` **and** that the
/// account may exercise it: `read` scopes only need coverage, while
/// `write`/`follow`/`push`/`admin` and their granular forms require an
/// approved account (`users.approved_at`) and a verified email when the
/// instance requires verification. `admin` additionally needs the
/// `admin` role. Epics 12+ call this on every protected route.
pub fn require_scope(config: &Config, auth: &AuthContext, required: &str) -> Result<()> {
    check_scope(config, auth, required, &["admin"])
}

/// [`require_scope`] variant for the epic-21 moderation surface:
/// `admin:*` scopes admit the `moderator` role as well as `admin`.
/// Admin-only surfaces (domain blocks, role changes, emoji admin)
/// keep using `require_scope`, which demands `role == "admin"`.
pub fn require_moderator_scope(config: &Config, auth: &AuthContext, required: &str) -> Result<()> {
    check_scope(config, auth, required, &["admin", "moderator"])
}

/// Shared scope + eligibility check. `admin:*` scopes additionally
/// require the account's role to be one of `staff_roles`.
fn check_scope(
    config: &Config,
    auth: &AuthContext,
    required: &str,
    staff_roles: &[&str],
) -> Result<()> {
    let scopes = ScopeSet::parse(&auth.scopes)
        .map_err(|e| Error::Validation(format!("token has invalid scopes: {e}")))?;
    if !scopes.covers(required) {
        return Err(Error::Forbidden);
    }
    let base = required.split(':').next().unwrap_or_default();
    if base == "admin" && !staff_roles.contains(&auth.role.as_str()) {
        return Err(Error::Forbidden);
    }
    if !is_read_scope(required) {
        if !auth.approved {
            return Err(Error::Forbidden);
        }
        if requires_email_verification(config) && !auth.email_verified {
            return Err(Error::Forbidden);
        }
    }
    Ok(())
}
