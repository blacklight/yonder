//! Two-factor authentication (epic 08): TOTP, backup codes and the
//! pending-login state machine. WebAuthn ceremonies live in
//! [`crate::auth::webauthn`]; both share the "2FA enabled" definition —
//! a `two_factor` row **or** ≥1 `webauthn_credentials` row.
//!
//! Secret material at rest:
//! - TOTP secrets are AEAD-encrypted (ChaCha20-Poly1305 under an
//!   HKDF-SHA256 key derived from `auth.secret_key`) — decrypted per
//!   verification, never logged, never returned after setup.
//! - Backup codes are stored **sha256-hex** of the normalized code —
//!   random high-entropy material doesn't need a KDF (same rule as
//!   bearer tokens), and deterministic equality is required for lookup.
//! - Pending ceremony state (TOTP setup secrets, pending logins) lives
//!   in Redis with a TTL, like the epic-07 OAuth flows.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine;
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use totp_rs::{Algorithm, Builder as TotpBuilder, Secret, Totp};

use crate::{
    auth::tokens::{get_secret_key, random_secret, set_secret_key, take_secret_key},
    config::Config,
    http::AppState,
    Error, Result,
};

// ---------------------------------------------------------------------------
// Constants / Redis domains
// ---------------------------------------------------------------------------

/// Redis domain for staged pending TOTP setups, keyed by user id.
const TOTP_SETUP_DOMAIN: &str = "2fa-totp-setup";
const TOTP_SETUP_TTL_SECONDS: u64 = 600;

/// Redis domain for password-verified logins waiting on a second
/// factor, keyed by the sha256 of the issued `mfa_token`.
const PENDING_LOGIN_DOMAIN: &str = "2fa-pending";
pub const PENDING_LOGIN_TTL_SECONDS: u64 = 300;
/// Failed second-factor attempts allowed before the pending login is
/// dropped and the password must be re-authenticated.
pub const PENDING_LOGIN_MAX_ATTEMPTS: u32 = 5;

/// Backup codes per batch (`xxxx-xxxx-xxxx` display format).
const BACKUP_CODE_COUNT: usize = 10;
/// RFC 6238 defaults — the authenticator-app triple.
const TOTP_STEP_SECONDS: u64 = 30;
const TOTP_DIGITS: u8 = 6;
const TOTP_SKEW: u16 = 1;

// ---------------------------------------------------------------------------
// Encryption at rest
// ---------------------------------------------------------------------------

/// The TOTP-secret AEAD key: `HKDF-SHA256(auth.secret_key,
/// salt="yonder:2fa", info="totp-secret")`. `auth.secret_key` is
/// optional in config but load-bearing here — enrollment fails with a
/// 500 (server misconfiguration, not a client error) when it's unset.
fn totp_aead_key(config: &Config) -> Result<[u8; 32]> {
    let Some(master) = config.auth.secret_key.as_deref().filter(|k| !k.is_empty()) else {
        tracing::error!("auth.secret_key is unset — 2FA enrollment is unavailable");
        return Err(Error::Internal(
            "two-factor authentication is not configured on this instance".into(),
        ));
    };
    let hkdf = Hkdf::<Sha256>::new(Some(b"yonder:2fa"), master.as_bytes());
    let mut key = [0u8; 32];
    hkdf.expand(b"totp-secret", &mut key)
        .map_err(|e| Error::Internal(format!("hkdf expand failed: {e}")))?;
    Ok(key)
}

/// `base64(nonce ‖ ciphertext ‖ tag)`.
fn encrypt_totp_secret(key: &[u8; 32], secret: &[u8]) -> Result<String> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| Error::Internal(format!("totp key init failed: {e}")))?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), secret)
        .map_err(|e| Error::Internal(format!("totp secret encryption failed: {e}")))?;
    let mut blob = Vec::with_capacity(nonce.len() + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

fn decrypt_totp_secret(key: &[u8; 32], stored: &str) -> Result<Vec<u8>> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .map_err(|e| Error::Internal(format!("stored totp secret is not base64: {e}")))?;
    if blob.len() <= 12 {
        return Err(Error::Internal("stored totp secret is truncated".into()));
    }
    let (nonce, ciphertext) = blob.split_at(12);
    ChaCha20Poly1305::new_from_slice(key)
        .map_err(|e| Error::Internal(format!("totp key init failed: {e}")))?
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|e| Error::Internal(format!("totp secret decryption failed: {e}")))
}

// ---------------------------------------------------------------------------
// TOTP primitives
// ---------------------------------------------------------------------------

/// A `Totp` over raw secret bytes — verification only, so no
/// issuer/account name is needed.
fn verify_totp_for(secret: &[u8]) -> Result<Totp> {
    TotpBuilder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(TOTP_DIGITS)
        .with_skew(TOTP_SKEW)
        .with_step_duration(TOTP_STEP_SECONDS)
        .with_secret(secret.to_vec())
        .build()
        .map_err(|e| Error::Internal(format!("totp build failed: {e}")))
}

/// A `Totp` carrying issuer/account metadata — needed for the
/// `otpauth://` URI and QR code shown at setup.
fn enrollment_totp_for(secret: &[u8], account_name: &str, issuer: &str) -> Result<Totp> {
    TotpBuilder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(TOTP_DIGITS)
        .with_skew(TOTP_SKEW)
        .with_step_duration(TOTP_STEP_SECONDS)
        .with_secret(secret.to_vec())
        .with_account_name(account_name.to_string())
        .with_issuer(Some(issuer.to_string()))
        .build()
        .map_err(|e| Error::Internal(format!("totp build failed: {e}")))
}

fn now_unix() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

// ---------------------------------------------------------------------------
// 2FA state
// ---------------------------------------------------------------------------

/// A user's enrolled second factors — the single query the login path
/// needs. "2FA enabled" = TOTP confirmed **or** ≥1 WebAuthn credential;
/// a staged (unconfirmed) TOTP setup does not count.
#[derive(Debug, Default, Clone)]
pub struct TwoFactorState {
    pub totp_enabled: bool,
    pub totp_last_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub webauthn_count: i64,
    pub backup_codes_remaining: i64,
}

impl TwoFactorState {
    pub fn enabled(&self) -> bool {
        self.totp_enabled || self.webauthn_count > 0
    }

    /// Methods that can complete a pending login — `backup_code` is
    /// listed only while an unused one remains.
    pub fn methods(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.totp_enabled {
            out.push("totp".to_string());
        }
        if self.webauthn_count > 0 {
            out.push("webauthn".to_string());
        }
        if self.backup_codes_remaining > 0 {
            out.push("backup_code".to_string());
        }
        out
    }
}

/// Load the user's 2FA enrollment state.
pub async fn two_factor_state(pool: &sqlx::PgPool, user_id: i64) -> Result<TwoFactorState> {
    let row = sqlx::query(
        "SELECT tf.user_id IS NOT NULL AS totp_enabled, \
                tf.last_verified_at, \
                (SELECT count(*) FROM webauthn_credentials w WHERE w.user_id = u.id) AS webauthn_count, \
                (SELECT count(*) FROM backup_codes b WHERE b.user_id = u.id AND b.used_at IS NULL) AS backup_codes_remaining \
         FROM users u LEFT JOIN two_factor tf ON tf.user_id = u.id \
         WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(Error::NotFound("user".into()));
    };
    Ok(TwoFactorState {
        totp_enabled: row.get("totp_enabled"),
        totp_last_verified_at: row.get("last_verified_at"),
        webauthn_count: row.get("webauthn_count"),
        backup_codes_remaining: row.get("backup_codes_remaining"),
    })
}

// ---------------------------------------------------------------------------
// Pending logins
// ---------------------------------------------------------------------------

/// A password-verified login waiting on its second factor. Staged in
/// Redis under [`PENDING_LOGIN_DOMAIN`], keyed by the sha256 of the
/// `mfa_token` handed to the client (see `tokens::set_secret_key`), so
/// the plaintext token never touches the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLogin {
    pub user_id: i64,
    /// `oauth_applications.id` when the original grant was client-bound.
    pub application_id: Option<i64>,
    /// The scope string negotiated by the original grant.
    pub scope: String,
    pub attempts: u32,
}

/// Stage a pending login and return the `mfa_token` for the
/// `mfa_required` response.
pub async fn stage_pending_login(
    state: &AppState,
    user_id: i64,
    application_id: Option<i64>,
    scope: &str,
) -> Result<String> {
    let mfa_token = random_secret();
    let pending = PendingLogin {
        user_id,
        application_id,
        scope: scope.to_string(),
        attempts: 0,
    };
    set_secret_key(
        state,
        PENDING_LOGIN_DOMAIN,
        &mfa_token,
        &json!(pending),
        PENDING_LOGIN_TTL_SECONDS,
    )
    .await?;
    Ok(mfa_token)
}

/// Read a pending login without consuming it (a second-factor attempt
/// must leave the entry alive for retries).
pub async fn pending_login(state: &AppState, mfa_token: &str) -> Result<Option<PendingLogin>> {
    get_secret_key(state, PENDING_LOGIN_DOMAIN, mfa_token).await
}

/// Consume a pending login after a successful second factor — atomic
/// GETDEL, so concurrent completions can't both issue tokens.
pub async fn consume_pending_login(
    state: &AppState,
    mfa_token: &str,
) -> Result<Option<PendingLogin>> {
    take_secret_key(state, PENDING_LOGIN_DOMAIN, mfa_token).await
}

/// Count a failed second-factor attempt against the pending login in a
/// single Lua round-trip — the counter increment, budget check and
/// TTL-preserving rewrite are atomic, so concurrent failures can't
/// undercount. The entry's original TTL is kept (a failure budget must
/// not extend the login's lifetime). At [`PENDING_LOGIN_MAX_ATTEMPTS`]
/// the entry is dropped; the client must re-authenticate the password.
/// Returns `true` when the pending login is dead (or never existed).
pub async fn fail_pending_login(state: &AppState, mfa_token: &str) -> Result<bool> {
    static SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
        redis::Script::new(
            "local v = redis.call('GET', KEYS[1])\n\
             if not v then return 1 end\n\
             local p = cjson.decode(v)\n\
             p.attempts = p.attempts + 1\n\
             if p.attempts >= tonumber(ARGV[1]) then\n\
             \tredis.call('DEL', KEYS[1])\n\
             \treturn 1\n\
             end\n\
             local ttl = redis.call('PTTL', KEYS[1])\n\
             if ttl < 0 then ttl = 1 end\n\
             redis.call('SET', KEYS[1], cjson.encode(p), 'PX', ttl)\n\
             return 0",
        )
    });
    let mut conn = state.redis.conn().await?;
    let key = state.redis.key(&format!(
        "{PENDING_LOGIN_DOMAIN}:{}",
        crate::http::middleware::auth::token_hash(mfa_token)
    ));
    let dead: bool = SCRIPT
        .clone()
        .key(key)
        .arg(PENDING_LOGIN_MAX_ATTEMPTS)
        .invoke_async(&mut *conn)
        .await?;
    Ok(dead)
}

// ---------------------------------------------------------------------------
// TOTP enrollment + verification
// ---------------------------------------------------------------------------

/// `POST /api/v1/2fa/totp/setup` — generate the pending secret, staged
/// in Redis (never written to the DB until confirmed). Returns the
/// base32 secret, `otpauth://` URI and a PNG QR data URI — the only
/// response that ever carries the plaintext secret.
pub async fn begin_totp_setup(state: &AppState, user_id: i64) -> Result<Value> {
    // Fail early on misconfiguration — encrypting at confirmation time
    // without the master key would strand the enrollment.
    totp_aead_key(&state.config())?;
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM two_factor WHERE user_id = $1)")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?
    {
        return Err(Error::Conflict(
            "TOTP is already enabled; remove it first".into(),
        ));
    }

    let row = sqlx::query(
        "SELECT a.username FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    let username: String = row.get("username");

    let mut secret = [0u8; 20];
    OsRng.fill_bytes(&mut secret);
    let secret_b32 = Secret::from(secret.to_vec()).to_base32();
    let totp = enrollment_totp_for(&secret, &username, &state.config().instance.domain)?;
    let otpauth_uri = totp
        .to_url()
        .map_err(|e| Error::Internal(format!("totp url failed: {e}")))?;
    let qr = totp
        .to_qr_base64()
        .map_err(|e| Error::Internal(format!("totp qr failed: {e}")))?;

    set_secret_key(
        state,
        TOTP_SETUP_DOMAIN,
        &user_id.to_string(),
        &json!({ "secret": secret_b32 }),
        TOTP_SETUP_TTL_SECONDS,
    )
    .await?;
    Ok(json!({
        "secret": secret_b32,
        "otpauth_uri": otpauth_uri,
        "qr_data_uri": format!("data:image/png;base64,{qr}"),
    }))
}

/// `POST /api/v1/2fa/totp/confirm` — verify a code against the pending
/// secret, persist it encrypted, and issue the first backup-code batch.
/// Returns the plaintext codes — shown exactly once.
pub async fn confirm_totp(state: &AppState, user_id: i64, code: &str) -> Result<Vec<String>> {
    let staged: Option<Value> =
        get_secret_key(state, TOTP_SETUP_DOMAIN, &user_id.to_string()).await?;
    let Some(staged) = staged else {
        return Err(Error::NotFound("pending TOTP setup".into()));
    };
    let secret_b32 = staged
        .get("secret")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Internal("pending TOTP setup is malformed".into()))?;
    let secret = Secret::try_from_base32(secret_b32)
        .map_err(|_| Error::Internal("pending TOTP secret is not base32".into()))?;
    let totp = verify_totp_for(secret.as_bytes())?;
    if totp.check(code.trim(), now_unix()).is_none() {
        return Err(Error::Validation("the verification code is invalid".into()));
    }

    // Claim the setup atomically before writing — a second concurrent
    // confirm gets `None` here instead of overwriting the first
    // confirm's just-issued backup-code batch. (On a DB failure below
    // the staged secret is gone; the user starts a fresh setup.)
    let claimed: Option<Value> =
        take_secret_key(state, TOTP_SETUP_DOMAIN, &user_id.to_string()).await?;
    if claimed.is_none() {
        return Err(Error::Conflict(
            "the pending TOTP setup was already consumed".into(),
        ));
    }

    let key = totp_aead_key(&state.config())?;
    let encrypted = encrypt_totp_secret(&key, secret.as_bytes())?;
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO two_factor (user_id, totp_secret) VALUES ($1, $2) \
         ON CONFLICT (user_id) DO UPDATE \
         SET totp_secret = EXCLUDED.totp_secret, enabled_at = now(), last_verified_at = NULL",
    )
    .bind(user_id)
    .bind(&encrypted)
    .execute(&mut *tx)
    .await?;
    let codes = write_backup_codes(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(codes)
}

/// Verify a TOTP code against the enrolled secret. Replay protection
/// is folded into the `last_verified_at` update: a code only verifies
/// when its matched timestep is newer than the last recorded one, and
/// the conditional UPDATE makes concurrent replays unable to both win.
pub async fn verify_totp(state: &AppState, user_id: i64, code: &str) -> Result<bool> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT totp_secret FROM two_factor WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let Some(stored) = stored else {
        return Ok(false);
    };
    let secret = match totp_aead_key(&state.config())
        .and_then(|key| decrypt_totp_secret(&key, &stored))
    {
        Ok(secret) => secret,
        Err(e) => {
            // The enrolled secret is unusable (master key unset or
            // rotated, or a corrupt blob) — TOTP can't verify, but a
            // backup code must still be able to complete the login.
            // The cause is logged for operators, never the client.
            tracing::warn!(error = %e, "totp verification unavailable; backup codes still apply");
            return Ok(false);
        }
    };
    let totp = verify_totp_for(&secret)?;
    let Some(step) = totp.check(code.trim(), now_unix()) else {
        return Ok(false);
    };
    let accepted: Option<i64> = sqlx::query_scalar(
        "UPDATE two_factor SET last_verified_at = now() \
         WHERE user_id = $1 \
           AND (last_verified_at IS NULL \
                OR floor(extract(epoch from last_verified_at) / $3) < $2) \
         RETURNING user_id",
    )
    .bind(user_id)
    .bind(step as i64)
    .bind(TOTP_STEP_SECONDS as i64)
    .fetch_optional(&state.db)
    .await?;
    Ok(accepted.is_some())
}

// ---------------------------------------------------------------------------
// Backup codes
// ---------------------------------------------------------------------------

/// `xxxx-xxxx-xxxx` — 12 hex chars grouped in fours (Songhive format).
fn generate_backup_code() -> String {
    let mut buf = [0u8; 6];
    OsRng.fill_bytes(&mut buf);
    let hexed = hex::encode(buf);
    format!("{}-{}-{}", &hexed[..4], &hexed[4..8], &hexed[8..])
}

/// Lowercase, separators stripped — `ABCD-EF01-2345`, `abcd ef01 2345`
/// and `abcdef012345` are the same code.
fn normalize_backup_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn backup_code_hash(code: &str) -> String {
    hex::encode(Sha256::digest(normalize_backup_code(code).as_bytes()))
}

/// Replace the user's batch inside `tx` and return the plaintext codes
/// (the only time they are ever visible).
async fn write_backup_codes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
) -> Result<Vec<String>> {
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    let codes: Vec<String> = (0..BACKUP_CODE_COUNT)
        .map(|_| generate_backup_code())
        .collect();
    for code in &codes {
        sqlx::query("INSERT INTO backup_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(backup_code_hash(code))
            .execute(&mut **tx)
            .await?;
    }
    Ok(codes)
}

/// `POST /api/v1/2fa/backup_codes` — a fresh batch; the old hashes are
/// deleted in the same transaction. Requires 2FA to be enabled (or an
/// existing batch retained via `keep_backup_codes`).
pub async fn regenerate_backup_codes(state: &AppState, user_id: i64) -> Result<Vec<String>> {
    let twofa = two_factor_state(&state.db, user_id).await?;
    if !twofa.enabled() && twofa.backup_codes_remaining == 0 {
        return Err(Error::Validation(
            "two-factor authentication is not enabled".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let codes = write_backup_codes(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(codes)
}

/// Single-use redemption: one atomic conditional UPDATE so concurrent
/// submissions of the same code can't both succeed.
pub async fn consume_backup_code(pool: &sqlx::PgPool, user_id: i64, code: &str) -> Result<bool> {
    let id: Option<i64> = sqlx::query_scalar(
        "UPDATE backup_codes SET used_at = now() \
         WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL \
         RETURNING id",
    )
    .bind(user_id)
    .bind(backup_code_hash(code))
    .fetch_optional(pool)
    .await?;
    Ok(id.is_some())
}

// ---------------------------------------------------------------------------
// Method removal / disable
// ---------------------------------------------------------------------------

/// Inside `tx`, drop the backup-code batch when no second factor is
/// left — the "disabling the last method invalidates the codes" rule
/// shared by the remove-TOTP/remove-key paths.
async fn drop_backup_codes_if_disabled(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
) -> Result<()> {
    let enabled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM two_factor WHERE user_id = $1) \
            OR EXISTS(SELECT 1 FROM webauthn_credentials WHERE user_id = $1)",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    if !enabled {
        sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// `DELETE /api/v1/2fa/totp` — remove TOTP; returns `false` when it
/// wasn't enabled. Backup codes die with it when no WebAuthn keys
/// remain.
pub async fn disable_totp(state: &AppState, user_id: i64) -> Result<bool> {
    let mut tx = state.db.begin().await?;
    let removed = sqlx::query("DELETE FROM two_factor WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    if removed {
        drop_backup_codes_if_disabled(&mut tx, user_id).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

/// `DELETE /api/v1/2fa/webauthn/credentials/{id}` — remove one key;
/// returns `false` when the id is unknown. Backup codes die when no
/// second factor remains.
pub async fn disable_webauthn_credential(
    state: &AppState,
    user_id: i64,
    credential_row_id: i64,
) -> Result<bool> {
    let mut tx = state.db.begin().await?;
    let removed = sqlx::query("DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2")
        .bind(credential_row_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    if removed {
        drop_backup_codes_if_disabled(&mut tx, user_id).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

/// `DELETE /api/v1/2fa` — disable every method. `keep_backup_codes` is
/// the deliberate escape hatch for retaining a batch with no methods
/// left; issued bearer tokens are unaffected (2FA gates login, not
/// token validity). Staged enrollment state is cleared too, so nothing
/// can be confirmed after the fact — pending logins and assertion
/// ceremonies are keyed by unknown tokens; they expire on their TTL and
/// can never complete once no method exists anyway.
pub async fn disable_all(state: &AppState, user_id: i64, keep_backup_codes: bool) -> Result<()> {
    let _: Option<Value> = take_secret_key(state, TOTP_SETUP_DOMAIN, &user_id.to_string()).await?;
    crate::auth::webauthn::clear_staged_registration(state, user_id).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM two_factor WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM webauthn_credentials WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if !keep_backup_codes {
        sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The admin primitive epic 22 wires a route onto: clear TOTP, every
/// WebAuthn credential and every backup code in one transaction.
pub async fn admin_disable_2fa(pool: &sqlx::PgPool, user_id: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    admin_disable_2fa_conn(&mut tx, user_id).await?;
    tx.commit().await?;
    Ok(())
}

/// [`admin_disable_2fa`] inside a caller-owned transaction — the CLI
/// commits the wipe atomically with its audit row so the mutation can
/// never land without its audit trail.
pub async fn admin_disable_2fa_conn(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM two_factor WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM webauthn_credentials WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Status + re-authentication
// ---------------------------------------------------------------------------

/// `GET /api/v1/2fa` — the account's 2FA posture for the management UI.
pub async fn status(state: &AppState, user_id: i64) -> Result<Value> {
    let twofa = two_factor_state(&state.db, user_id).await?;
    let credentials = crate::auth::webauthn::list_credentials(&state.db, user_id).await?;
    Ok(json!({
        "enabled": twofa.enabled(),
        "methods": twofa.methods(),
        "totp": {
            "enabled": twofa.totp_enabled,
            "last_verified_at": twofa.totp_last_verified_at.map(|t| t.to_rfc3339()),
        },
        "webauthn": credentials,
        "backup_codes_remaining": twofa.backup_codes_remaining,
    }))
}

/// Re-authentication for destructive 2FA operations — the account
/// password in the request body (Songhive pattern). `Forbidden` on
/// mismatch, matching the wrong-password-is-403 rule.
pub async fn require_password(pool: &sqlx::PgPool, user_id: i64, password: &str) -> Result<()> {
    let hash: Option<String> = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .flatten();
    if hash.is_some_and(|h| crate::auth::password::verify_password(password, &h)) {
        Ok(())
    } else {
        Err(Error::Forbidden("incorrect password"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_key(secret: Option<&str>) -> Config {
        let mut config = Config::default();
        config.auth.secret_key = secret.map(String::from);
        config
    }

    #[test]
    fn totp_secret_roundtrips_through_aead() {
        let key = [7u8; 32];
        let blob = encrypt_totp_secret(&key, b"JBSWY3DPEHPK3PXPJBSWY3DP").unwrap();
        // Nonce + ciphertext + tag — never the plaintext.
        assert!(!blob.contains("JBSWY3DPEHPK3PXPJBSWY3DP"));
        assert_eq!(
            decrypt_totp_secret(&key, &blob).unwrap(),
            b"JBSWY3DPEHPK3PXPJBSWY3DP"
        );
    }

    #[test]
    fn totp_secret_rejects_wrong_key_and_garbage() {
        let key = [7u8; 32];
        let blob = encrypt_totp_secret(&key, b"secretbytes").unwrap();
        assert!(decrypt_totp_secret(&[8u8; 32], &blob).is_err());
        assert!(decrypt_totp_secret(&key, "not-base64!!!").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 8]);
        assert!(decrypt_totp_secret(&key, &short).is_err());
    }

    #[test]
    fn totp_aead_key_requires_secret_key() {
        assert!(totp_aead_key(&config_with_key(None)).is_err());
        assert!(totp_aead_key(&config_with_key(Some(""))).is_err());
        let key = totp_aead_key(&config_with_key(Some("master"))).unwrap();
        // Domain-separated: a different master yields a different key.
        let other = totp_aead_key(&config_with_key(Some("other"))).unwrap();
        assert_ne!(key, other);
    }

    #[test]
    fn backup_code_hash_normalizes() {
        assert_eq!(
            backup_code_hash("ABCD-EF01-2345"),
            backup_code_hash("abcd ef01 2345"),
        );
        assert_ne!(
            backup_code_hash("abcd-ef01-2345"),
            backup_code_hash("abcd-ef01-2346")
        );
    }
}
