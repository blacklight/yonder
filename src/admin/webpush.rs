//! WebPush key administration (epic 22): generate a VAPID P-256
//! keypair and store it as `http.vapid_public_key` /
//! `http.vapid_private_key` overlays. Push delivery itself lands in a
//! later epic — this module only mints, stores and audits the keys.
//!
//! Encodings follow the WebPush convention (RFC 8292): the public key
//! is the 65-byte uncompressed point, base64url unpadded; the private
//! key is the 32-byte scalar, base64url unpadded.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::{
    elliptic_curve::{sec1::ToSec1Point, Generate},
    SecretKey,
};
use serde_json::{json, Value};

use crate::{db, http::AppState, Result};

/// Generate a fresh VAPID keypair, returning `(public, private)` in
/// WebPush base64url encoding.
pub fn generate() -> (String, String) {
    let secret = SecretKey::generate();
    let private = URL_SAFE_NO_PAD.encode(secret.to_bytes());
    let public = URL_SAFE_NO_PAD.encode(secret.public_key().to_sec1_point(false).as_bytes());
    (public, private)
}

/// Generate a keypair, store both halves as overlays, reload the
/// effective config and audit the rotation. The response carries only
/// the public key and the configured `mailto:` subject — the private
/// key is write-only everywhere.
pub async fn generate_and_store(state: &AppState, actor_account_id: Option<i64>) -> Result<Value> {
    let (public, private) = generate();
    db::config_overlays::upsert_many(
        &state.db,
        &[
            ("http.vapid_public_key", json!(public)),
            ("http.vapid_private_key", json!(private)),
        ],
    )
    .await?;
    state.reload_config().await?;
    crate::moderation::audit::record(
        &state.db,
        actor_account_id,
        "webpush.generate_keys",
        "config",
        None,
        json!({ "private_key": "<redacted>" }),
    )
    .await?;
    Ok(json!({
        "public_key": public,
        "subject": state.config().http.webpush_subject,
    }))
}
