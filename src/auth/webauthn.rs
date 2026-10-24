//! WebAuthn/FIDO2 second-factor ceremonies (epic 08) — hardware
//! security keys (YubiKey-class) as a *second* factor after password.
//! Passkey/discoverable-credential passwordless login is deliberately
//! out of scope: passkeys are designed to *replace* passwords.
//!
//! Nothing secret is stored: `webauthn_credentials.public_key` holds
//! the serde-JSON serialization of the `SecurityKey` webauthn-rs
//! returns (COSE public key + signature counter + transports — the
//! crate documents it as safe to serialize to a database), and
//! `credential_id` holds the raw cred-id bytes for the UNIQUE
//! constraint and allow-list lookups.

use serde_json::Value;
use sqlx::Row;
use webauthn_rs::prelude::*;

use crate::{
    auth::tokens::{set_secret_key, take_secret_key},
    auth::two_factor::PENDING_LOGIN_TTL_SECONDS,
    config::Config,
    http::AppState,
    Error, Result,
};

/// Redis domain for registration ceremony state, keyed by user id.
const REG_DOMAIN: &str = "2fa-webauthn-reg";
/// Redis domain for assertion ceremony state, keyed by the sha256 of
/// the pending login's `mfa_token`.
const AUTH_DOMAIN: &str = "2fa-webauthn-auth";
const REG_TTL_SECONDS: u64 = 600;

/// Build the relying party — cheap enough to construct per ceremony.
/// `rp_id` is `instance.domain`; the origin is `federation.base_url`
/// (default `https://{domain}`). webauthn-rs requires rp_id to be an
/// effective domain of the origin, so the pair must be consistent —
/// `http://localhost` dev works because `localhost` is a valid rp_id.
fn relying_party(config: &Config) -> Result<Webauthn> {
    let rp_id = config.instance.domain.as_str();
    let origin_url = crate::activitypub::federation_base_url(config);
    let origin = Url::parse(&origin_url)
        .map_err(|e| Error::Internal(format!("invalid federation base_url: {e}")))?;
    WebauthnBuilder::new(rp_id, &origin)
        .map_err(|e| {
            tracing::error!(error = ?e, rp_id, %origin, "webauthn relying-party misconfiguration");
            Error::Internal("webauthn relying-party misconfiguration".into())
        })?
        .rp_name(&config.instance.name)
        .build()
        .map_err(|e| Error::Internal(format!("webauthn build failed: {e}")))
}

/// The webauthn-rs 0.5 user entity: `user_unique_id` is a stable UUID
/// derived from `users.id`; `user_name`/`user_display_name` are the
/// local account's handles.
async fn user_entity(pool: &sqlx::PgPool, user_id: i64) -> Result<(Uuid, String, String)> {
    let row = sqlx::query(
        "SELECT a.username, a.display_name \
         FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::NotFound("user".into()))?;
    let username: String = row.get("username");
    let display: String = row
        .get::<Option<String>, _>("display_name")
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| username.clone());
    Ok((Uuid::from_u128(user_id as u128), username, display))
}

/// A stored `webauthn_credentials` row with its deserialized key.
struct StoredCredential {
    security_key: SecurityKey,
}

/// Load the user's credentials, deserializing each `public_key` blob
/// back into a `SecurityKey`.
async fn load_credentials(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<StoredCredential>> {
    let rows = sqlx::query("SELECT public_key FROM webauthn_credentials WHERE user_id = $1")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| {
            let blob: &[u8] = row.get("public_key");
            serde_json::from_slice::<SecurityKey>(blob)
                .map(|security_key| StoredCredential { security_key })
                .map_err(|e| Error::Internal(format!("stored webauthn key is corrupt: {e}")))
        })
        .collect()
}

fn internal(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Internal(format!("{context}: {e}"))
}

/// Trim + enforce the column's `char_length(name) <= 500` CHECK —
/// returns the normalized name (`""` means unnamed at registration;
/// rename additionally rejects empty).
fn normalize_credential_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.chars().count() > 500 {
        return Err(Error::Validation(
            "credential name is too long (max 500 chars)".into(),
        ));
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// Registration ceremony
// ---------------------------------------------------------------------------

/// `POST /api/v1/2fa/webauthn/register/begin` — the creation options
/// for the user agent; ceremony state is staged in Redis.
pub async fn begin_registration(state: &AppState, user_id: i64) -> Result<Value> {
    let (uuid, username, display) = user_entity(&state.db, user_id).await?;
    let existing = load_credentials(&state.db, user_id).await?;
    let exclude: Vec<CredentialID> = existing
        .iter()
        .map(|c| c.security_key.cred_id().clone())
        .collect();
    let rp = relying_party(&state.config())?;
    let (ccr, skr) = rp
        .start_securitykey_registration(uuid, &username, &display, Some(exclude), None, None)
        .map_err(|e| internal("webauthn registration begin failed", e))?;
    set_secret_key(
        state,
        REG_DOMAIN,
        &user_id.to_string(),
        &serde_json::to_value(&skr).map_err(|e| internal("ceremony state", e))?,
        REG_TTL_SECONDS,
    )
    .await?;
    serde_json::to_value(&ccr).map_err(|e| internal("challenge response", e))
}

/// Drop a staged registration ceremony for `user_id` — called when the
/// account's 2FA is disabled so a pending ceremony can't be finished
/// afterwards.
pub async fn clear_staged_registration(state: &AppState, user_id: i64) -> Result<()> {
    let _: Option<Value> = take_secret_key(state, REG_DOMAIN, &user_id.to_string()).await?;
    Ok(())
}

/// `POST /api/v1/2fa/webauthn/register` — verify the attestation and
/// persist the credential under `name` (`""` → unnamed). The ceremony
/// state is single-use — a failed finish requires a fresh begin.
/// Returns the new row id.
pub async fn finish_registration(
    state: &AppState,
    user_id: i64,
    name: &str,
    credential: &RegisterPublicKeyCredential,
) -> Result<i64> {
    // Validate before claiming the ceremony state so a bad name leaves
    // it alive for a corrected retry.
    let name = normalize_credential_name(name)?;
    let staged: Option<SecurityKeyRegistration> =
        take_secret_key(state, REG_DOMAIN, &user_id.to_string()).await?;
    let Some(skr) = staged else {
        return Err(Error::NotFound("webauthn registration ceremony".into()));
    };
    let rp = relying_party(&state.config())?;
    let key = rp
        .finish_securitykey_registration(credential, &skr)
        .map_err(|e| Error::Validation(format!("webauthn registration failed: {e}")))?;
    insert_credential(&state.db, user_id, key.cred_id().as_slice(), &key, &name).await
}

/// Persist a verified `SecurityKey` under `name`; the `credential_id`
/// UNIQUE constraint maps a duplicate to 409 Conflict. Returns the new
/// row id.
pub async fn insert_credential(
    pool: &sqlx::PgPool,
    user_id: i64,
    credential_id: &[u8],
    key: &SecurityKey,
    name: &str,
) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO webauthn_credentials (user_id, credential_id, public_key, name) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(user_id)
    .bind(credential_id)
    .bind(serde_json::to_vec(key).map_err(|e| internal("serialize security key", e))?)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::Conflict("this security key is already registered".into())
        }
        other => Error::Database(other),
    })
}

// ---------------------------------------------------------------------------
// Assertion ceremony (login)
// ---------------------------------------------------------------------------

/// `POST /oauth/2fa/webauthn/assert/begin` — challenge options for the
/// pending login's user; staged with the pending login's TTL.
pub async fn begin_authentication(
    state: &AppState,
    mfa_token: &str,
    user_id: i64,
) -> Result<Value> {
    let credentials = load_credentials(&state.db, user_id).await?;
    if credentials.is_empty() {
        return Err(Error::NotFound("webauthn credentials".into()));
    }
    let keys: Vec<SecurityKey> = credentials.into_iter().map(|c| c.security_key).collect();
    let rp = relying_party(&state.config())?;
    let (rcr, ast) = rp
        .start_securitykey_authentication(&keys)
        .map_err(|e| internal("webauthn authentication begin failed", e))?;
    set_secret_key(
        state,
        AUTH_DOMAIN,
        mfa_token,
        &serde_json::to_value(&ast).map_err(|e| internal("ceremony state", e))?,
        PENDING_LOGIN_TTL_SECONDS,
    )
    .await?;
    serde_json::to_value(&rcr).map_err(|e| internal("challenge response", e))
}

/// `POST /oauth/2fa/webauthn/assert` — verify the assertion; on success
/// the matched credential's counter is updated (clone detection) and
/// `last_used_at` bumped. `Ok(false)` is a verification failure — the
/// caller counts it against the pending-login attempt budget.
pub async fn finish_authentication(
    state: &AppState,
    mfa_token: &str,
    user_id: i64,
    credential: &PublicKeyCredential,
) -> Result<bool> {
    let staged: Option<SecurityKeyAuthentication> =
        take_secret_key(state, AUTH_DOMAIN, mfa_token).await?;
    let Some(ast) = staged else {
        return Err(Error::NotFound("webauthn authentication ceremony".into()));
    };
    let rp = relying_party(&state.config())?;
    let result = match rp.finish_securitykey_authentication(credential, &ast) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, "webauthn assertion rejected");
            return Ok(false);
        }
    };

    // Persist the counter update on the matched credential. The
    // credential may have been removed since the ceremony began — the
    // assertion is then simply a failure, not a server error.
    let cred_id = result.cred_id().as_slice().to_vec();
    let mut credentials = load_credentials(&state.db, user_id).await?;
    let Some(stored) = credentials
        .iter_mut()
        .find(|c| c.security_key.cred_id().as_slice() == cred_id)
    else {
        tracing::warn!("asserted webauthn credential is no longer stored");
        return Ok(false);
    };
    stored.security_key.update_credential(&result);
    sqlx::query(
        "UPDATE webauthn_credentials SET public_key = $3, last_used_at = now() \
         WHERE user_id = $1 AND credential_id = $2",
    )
    .bind(user_id)
    .bind(&cred_id)
    .bind(
        serde_json::to_vec(&stored.security_key)
            .map_err(|e| internal("serialize security key", e))?,
    )
    .execute(&state.db)
    .await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Credential management
// ---------------------------------------------------------------------------

/// `GET /api/v1/2fa/webauthn/credentials` — `{id, name, created_at,
/// last_used_at}` per key (newest first).
pub async fn list_credentials(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, name, created_at, last_used_at \
         FROM webauthn_credentials WHERE user_id = $1 ORDER BY id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.get::<i64, _>("id").to_string(),
                "name": row.get::<String, _>("name"),
                "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
                "last_used_at": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_used_at").map(|t| t.to_rfc3339()),
            })
        })
        .collect())
}

/// `POST /api/v1/2fa/webauthn/credentials/{id}/rename`.
pub async fn rename_credential(
    pool: &sqlx::PgPool,
    user_id: i64,
    credential_row_id: i64,
    name: &str,
) -> Result<bool> {
    let name = normalize_credential_name(name)?;
    if name.is_empty() {
        return Err(Error::Validation("credential name is required".into()));
    }
    Ok(
        sqlx::query("UPDATE webauthn_credentials SET name = $3 WHERE id = $1 AND user_id = $2")
            .bind(credential_row_id)
            .bind(user_id)
            .bind(name)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}
