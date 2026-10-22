//! Argon2id password hashing — the only KDF used for low-entropy user
//! secrets. High-entropy random material (tokens, client secrets) is
//! stored sha256-hex instead (see `crate::auth::tokens`).

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

use crate::Result;

/// Minimum accepted password length at registration.
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// Hash a plaintext password as an argon2id PHC string (`$argon2id$…`).
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| crate::Error::Internal(format!("password hashing failed: {e}")))?
        .to_string())
}

/// Verify a plaintext password against a stored PHC string.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}
