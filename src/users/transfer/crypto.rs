//! AEAD protection for remote OAuth credentials (epic 31 §9):
//! `oauth_connections` token/client-secret material is encrypted at
//! rest with XChaCha20-Poly1305 under an application-specific key
//! domain HKDF-derived from `auth.secret_key`. No key configured →
//! fail closed: remote imports are unavailable rather than storing
//! plaintext secrets.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::{config::Config, Error, Result};

/// Application key domain — changing it invalidates stored blobs.
const HKDF_INFO: &[u8] = b"yonder/oauth-connections/v1";

fn cipher(config: &Config) -> Result<XChaCha20Poly1305> {
    let secret = config
        .auth
        .secret_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Internal("auth.secret_key is required for remote import credentials".into())
        })?;
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(HKDF_INFO), secret.as_bytes())
        .expand(b"aead-key", &mut key)
        .map_err(|_| Error::Internal("oauth key derivation failed".into()))?;
    Ok(XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(
        &key,
    )))
}

/// Whether remote import credentials can be stored at all.
pub fn available(config: &Config) -> bool {
    cipher(config).is_ok()
}

/// `nonce(24) || ciphertext` — self-contained per blob.
pub fn encrypt(config: &Config, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = cipher(config)?;
    let nonce_bytes: [u8; 24] = rand_bytes();
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| Error::Internal("credential encryption failed".into()))?;
    let mut out = Vec::with_capacity(24 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Inverse of [`encrypt`].
pub fn decrypt(config: &Config, blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 24 {
        return Err(Error::Internal("credential blob too short".into()));
    }
    let cipher = cipher(config)?;
    cipher
        .decrypt(XNonce::from_slice(&blob[..24]), &blob[24..])
        .map_err(|_| Error::Internal("credential decryption failed".into()))
}

/// `encrypt` for nullable columns.
pub fn encrypt_opt(config: &Config, plaintext: Option<&str>) -> Result<Option<Vec<u8>>> {
    plaintext.map(|p| encrypt(config, p.as_bytes())).transpose()
}

/// Randomness straight from the OS CSPRNG (nonces, tokens, PKCE).
fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    // Infallible in practice: getrandom only fails when the OS
    // entropy source is unavailable, which also breaks TLS.
    getrandom::fill(&mut out).expect("OS entropy source unavailable");
    out
}

/// 64-char lowercase-hex random token (state, PKCE-adjacent ids).
pub fn rand_token() -> String {
    hex::encode(rand_bytes::<32>())
}

/// A PKCE S256 code challenge + its verifier.
pub fn pkce_pair() -> (String, String) {
    use base64::Engine;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand_bytes::<32>());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
