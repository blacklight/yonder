//! Authentication (epic 07): registration, OAuth2, tokens and sessions.
//!
//! - [`password`] — argon2id hashing for user passwords.
//! - [`scopes`] — the Mastodon scope model + the account-state gate
//!   (`require_scope`) used by protected routes.
//! - [`registration`] — `POST /api/v1/accounts` validation and the
//!   account → user → actor transaction.
//! - [`oauth`] — applications, client auth, authorization codes (+PKCE),
//!   device flow, authorized-app revocation; `GrantError` renders the
//!   doorkeeper error shape.
//! - [`tokens`] — bearer material (random, sha256-hex stored), token-pair
//!   issuance, app tokens, refresh rotation, Redis secret keys.
//! - [`sessions`] — interactive-login tracking (browser/IP/activity).

pub mod oauth;
pub mod password;
pub mod registration;
pub mod scopes;
pub mod sessions;
pub mod tokens;
