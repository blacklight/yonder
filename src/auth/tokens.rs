//! Token material and issuance.
//!
//! All bearer material is a 32-byte `OsRng` secret, hex-encoded and
//! stored **sha256-hex** (`crate::http::middleware::auth::token_hash`)
//! — a KDF is pointless for uniformly random secrets and would put
//! argon2id cost on every request. Plaintext tokens are returned to the
//! client exactly once.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use redis::AsyncCommands;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::oauth::{invalid_client, invalid_grant, AppRow, GrantError},
    http::AppState,
    Result,
};

/// A fresh 256-bit bearer secret, hex-encoded.
pub fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Browser-session metadata captured at an interactive login.
#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub user_agent: Option<String>,
    pub ip: Option<String>,
}

/// A freshly issued access(+refresh) token pair.
#[derive(Debug)]
pub struct IssuedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub access_token_id: i64,
    pub scope: String,
    /// Unix timestamp for the Mastodon Token entity.
    pub created_at: i64,
    pub expires_in: u64,
}

/// Mastodon Token entity returned by `/oauth/token` and registration.
pub fn token_entity(issued: &IssuedTokens) -> Value {
    let mut entity = json!({
        "access_token": issued.access_token,
        "token_type": "Bearer",
        "scope": issued.scope,
        "created_at": issued.created_at,
        "expires_in": issued.expires_in,
    });
    if let Some(refresh) = &issued.refresh_token {
        entity["refresh_token"] = json!(refresh);
    }
    entity
}

/// Issue an access token (+ refresh token for user tokens) and persist
/// hashed rows. When `session` is set a `sessions` row wraps the new
/// access token — the session's `token_hash` mirrors the token's hash
/// (the column is `NOT NULL UNIQUE`) and `access_token_id` links them.
/// Both inserts share a transaction.
pub async fn issue_token_pair(
    state: &AppState,
    application_id: Option<i64>,
    user_id: i64,
    scopes: &str,
    name: Option<&str>,
    session: Option<&SessionMeta>,
) -> Result<IssuedTokens> {
    let mut tx = state.db.begin().await?;
    let issued = issue_token_pair_conn(
        &mut tx,
        &state.config(),
        application_id,
        user_id,
        scopes,
        name,
        session,
    )
    .await?;
    tx.commit().await?;
    Ok(issued)
}

/// [`issue_token_pair`] inside a caller-managed transaction — used by
/// registration so the account and its first token commit (or roll
/// back) together.
pub async fn issue_token_pair_conn(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &crate::config::Config,
    application_id: Option<i64>,
    user_id: i64,
    scopes: &str,
    name: Option<&str>,
    session: Option<&SessionMeta>,
) -> Result<IssuedTokens> {
    let access_token = random_secret();
    let refresh_token = random_secret();
    let access_hash = crate::http::middleware::auth::token_hash(&access_token);
    let refresh_hash = crate::http::middleware::auth::token_hash(&refresh_token);
    let expires_in = config.auth.access_token_expiry_minutes * 60;

    let access_token_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_access_tokens \
         (application_id, user_id, token_hash, scopes, name, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + $6::int * interval '1 minute') \
         RETURNING id",
    )
    .bind(application_id)
    .bind(user_id)
    .bind(&access_hash)
    .bind(scopes)
    .bind(name)
    .bind(config.auth.access_token_expiry_minutes as i64)
    .fetch_one(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO oauth_refresh_tokens (access_token_id, token_hash, expires_at) \
         VALUES ($1, $2, now() + $3::int * interval '1 day')",
    )
    .bind(access_token_id)
    .bind(&refresh_hash)
    .bind(config.auth.refresh_token_expiry_days as i64)
    .execute(&mut **tx)
    .await?;

    if let Some(meta) = session {
        sqlx::query(
            "INSERT INTO sessions (user_id, token_hash, access_token_id, user_agent, ip) \
             VALUES ($1, $2, $3, $4, $5::inet)",
        )
        .bind(user_id)
        .bind(&access_hash)
        .bind(access_token_id)
        .bind(&meta.user_agent)
        .bind(&meta.ip)
        .execute(&mut **tx)
        .await?;
    }

    Ok(IssuedTokens {
        access_token,
        refresh_token: Some(refresh_token),
        access_token_id,
        scope: scopes.to_string(),
        created_at: now_unix(),
        expires_in,
    })
}

/// Issue a `client_credentials` application token (`user_id NULL`,
/// no refresh token, no session).
pub async fn issue_app_token(
    state: &AppState,
    application_id: i64,
    scopes: &str,
) -> Result<IssuedTokens> {
    let access_token = random_secret();
    let access_hash = crate::http::middleware::auth::token_hash(&access_token);
    let expires_in = state.config().auth.access_token_expiry_minutes * 60;
    let access_token_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_access_tokens \
         (application_id, user_id, token_hash, scopes, expires_at) \
         VALUES ($1, NULL, $2, $3, now() + $4::int * interval '1 minute') \
         RETURNING id",
    )
    .bind(application_id)
    .bind(&access_hash)
    .bind(scopes)
    .bind(state.config().auth.access_token_expiry_minutes as i64)
    .fetch_one(&state.db)
    .await?;
    Ok(IssuedTokens {
        access_token,
        refresh_token: None,
        access_token_id,
        scope: scopes.to_string(),
        created_at: now_unix(),
        expires_in,
    })
}

/// An authenticated `client_credentials` token — the app-level
/// principal used by `apps/verify_credentials` and app-bound
/// registration. Kept distinct from `AuthContext`, which is user-only.
#[derive(Debug, Clone)]
pub struct AppAuth {
    pub application_id: i64,
    pub client_id: String,
    pub name: String,
    pub website: Option<String>,
    pub redirect_uris: Vec<String>,
    pub scopes: String,
}

/// Resolve a bearer token as an application token: `user_id IS NULL`,
/// joined to the owning application. Not the same lookup as
/// `resolve_token`, which intentionally only accepts user tokens.
/// `Ok(None)` means unknown/revoked/expired (a 401); `Err` is an
/// infrastructure failure the caller must surface, not collapse to 401.
pub async fn resolve_app_token(state: &AppState, token: &str) -> Result<Option<AppAuth>> {
    let row = sqlx::query(
        "SELECT t.application_id, t.scopes, ap.client_id, ap.name, \
                ap.website, ap.redirect_uris \
         FROM oauth_access_tokens t \
         JOIN oauth_applications ap ON ap.id = t.application_id \
         WHERE t.token_hash = $1 \
           AND t.user_id IS NULL \
           AND t.revoked_at IS NULL \
           AND (t.expires_at IS NULL OR t.expires_at > now())",
    )
    .bind(crate::http::middleware::auth::token_hash(token))
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|row| AppAuth {
        application_id: row.get("application_id"),
        client_id: row.get("client_id"),
        name: row.get("name"),
        website: row.get("website"),
        redirect_uris: row.get("redirect_uris"),
        scopes: row.get("scopes"),
    }))
}

/// Rotate a refresh token: claim the row under `FOR UPDATE`, check
/// expiry/revocation plus (for app-bound grants) the surviving
/// `oauth_authorizations` row, revoke the old pair, issue a new one
/// with identical app/user/scopes, and re-point any session at the new
/// access token. One transaction — concurrent rotations and revokes
/// cannot both succeed.
pub async fn rotate_refresh_token(
    state: &AppState,
    raw_refresh: &str,
    client: Option<&AppRow>,
) -> std::result::Result<IssuedTokens, GrantError> {
    let hash = crate::http::middleware::auth::token_hash(raw_refresh);
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;

    let row = sqlx::query(
        "SELECT r.id AS refresh_id, r.revoked_at AS refresh_revoked, \
                t.id AS access_id, t.user_id, t.application_id, t.scopes, t.revoked_at AS access_revoked \
         FROM oauth_refresh_tokens r \
         JOIN oauth_access_tokens t ON t.id = r.access_token_id \
         WHERE r.token_hash = $1 FOR UPDATE OF r, t",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;
    let Some(row) = row else {
        return Err(invalid_grant("The provided refresh token is invalid."));
    };
    if row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("refresh_revoked")
        .is_some()
        || row
            .get::<Option<chrono::DateTime<chrono::Utc>>, _>("access_revoked")
            .is_some()
    {
        return Err(invalid_grant("The provided refresh token is revoked."));
    }
    let expiry_ok: bool = sqlx::query_scalar(
        "SELECT expires_at IS NULL OR expires_at > now() FROM oauth_refresh_tokens WHERE id = $1",
    )
    .bind(row.get::<i64, _>("refresh_id"))
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;
    if !expiry_ok {
        return Err(invalid_grant("The provided refresh token is expired."));
    }

    let token_app: Option<i64> = row.get("application_id");
    if let Some(app_id) = token_app {
        match client {
            Some(client) if client.id == app_id => {}
            Some(_) => return Err(invalid_client("Client authentication failed.")),
            // RFC 6749 §6: a confidential client's refresh token may only
            // be rotated with client authentication; public app tokens
            // (and anonymous grants) rotate on the bearer secret alone.
            None => {
                let confidential: bool =
                    sqlx::query_scalar("SELECT confidential FROM oauth_applications WHERE id = $1")
                        .bind(app_id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(|e| GrantError::server(&e.to_string()))?;
                if confidential {
                    return Err(invalid_client("Client authentication failed."));
                }
            }
        }
    }
    let user_id: i64 = row.get("user_id");
    if let Some(app_id) = token_app {
        let authorized: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM oauth_authorizations \
             WHERE user_id = $1 AND application_id = $2)",
        )
        .bind(user_id)
        .bind(app_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
        if !authorized {
            return Err(invalid_grant("The authorization was revoked."));
        }
    }

    let access_id: i64 = row.get("access_id");
    sqlx::query("UPDATE oauth_access_tokens SET revoked_at = now() WHERE id = $1")
        .bind(access_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    sqlx::query("UPDATE oauth_refresh_tokens SET revoked_at = now() WHERE id = $1")
        .bind(row.get::<i64, _>("refresh_id"))
        .execute(&mut *tx)
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;

    let scopes: String = row.get("scopes");
    let access_token = random_secret();
    let refresh_token = random_secret();
    let access_hash = crate::http::middleware::auth::token_hash(&access_token);
    let refresh_hash = crate::http::middleware::auth::token_hash(&refresh_token);
    let expires_in = state.config().auth.access_token_expiry_minutes * 60;

    let new_access_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_access_tokens \
         (application_id, user_id, token_hash, scopes, expires_at) \
         VALUES ($1, $2, $3, $4, now() + $5::int * interval '1 minute') RETURNING id",
    )
    .bind(token_app)
    .bind(user_id)
    .bind(&access_hash)
    .bind(&scopes)
    .bind(state.config().auth.access_token_expiry_minutes as i64)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;
    sqlx::query(
        "INSERT INTO oauth_refresh_tokens (access_token_id, token_hash, expires_at) \
         VALUES ($1, $2, now() + $3::int * interval '1 day')",
    )
    .bind(new_access_id)
    .bind(&refresh_hash)
    .bind(state.config().auth.refresh_token_expiry_days as i64)
    .execute(&mut *tx)
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;

    // The session wraps the access token — re-link it to the
    // replacement so a refresh does not log the user out.
    sqlx::query(
        "UPDATE sessions SET access_token_id = $1, token_hash = $2 WHERE access_token_id = $3",
    )
    .bind(new_access_id)
    .bind(&access_hash)
    .bind(access_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| GrantError::server(&e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| GrantError::server(&e.to_string()))?;
    Ok(IssuedTokens {
        access_token,
        refresh_token: Some(refresh_token),
        access_token_id: new_access_id,
        scope: scopes,
        created_at: now_unix(),
        expires_in,
    })
}

/// Upsert the (user, application) authorization row that backs the
/// authorized-applications list; called on every successful user grant.
/// Accepts any executor so callers can run it inside a transaction.
pub async fn record_authorization<'e, E>(
    executor: E,
    user_id: i64,
    application_id: i64,
    scopes: &str,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO oauth_authorizations (user_id, application_id, scopes) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (user_id, application_id) \
         DO UPDATE SET scopes = EXCLUDED.scopes, updated_at = now()",
    )
    .bind(user_id)
    .bind(application_id)
    .bind(scopes)
    .execute(executor)
    .await?;
    Ok(())
}

/// Revoke every access token (and their refresh tokens) held by a
/// (user, application) pair — used by authorized-app revocation. Linked
/// `sessions` rows are deleted: they wrap the now-dead access tokens
/// and would otherwise linger in `GET /api/v1/sessions`.
pub async fn revoke_app_tokens(
    pool: &sqlx::PgPool,
    user_id: i64,
    application_id: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = now() \
         WHERE access_token_id IN ( \
             SELECT id FROM oauth_access_tokens \
             WHERE user_id = $1 AND application_id = $2)",
    )
    .bind(user_id)
    .bind(application_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE oauth_access_tokens SET revoked_at = now() \
         WHERE user_id = $1 AND application_id = $2",
    )
    .bind(user_id)
    .bind(application_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM sessions WHERE access_token_id IN ( \
             SELECT id FROM oauth_access_tokens \
             WHERE user_id = $1 AND application_id = $2)",
    )
    .bind(user_id)
    .bind(application_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM oauth_authorizations WHERE user_id = $1 AND application_id = $2")
        .bind(user_id)
        .bind(application_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Store a Redis value keyed on the sha256 of a bearer secret, with TTL.
pub async fn set_secret_key(
    state: &AppState,
    domain: &str,
    secret: &str,
    value: &Value,
    ttl_seconds: u64,
) -> Result<()> {
    let mut conn = state.redis.conn().await?;
    let key = state.redis.key(&format!(
        "{domain}:{}",
        crate::http::middleware::auth::token_hash(secret)
    ));
    let encoded = serde_json::to_string(value)
        .map_err(|e| crate::Error::Internal(format!("serialize redis value: {e}")))?;
    conn.set_ex::<_, _, ()>(key, encoded, ttl_seconds).await?;
    Ok(())
}

/// Atomic GET+DEL — `GETDEL` needs Redis 6.2; the Lua script works on
/// every supported version.
static GETDEL_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        "local v = redis.call('GET', KEYS[1])\n\
             if v then redis.call('DEL', KEYS[1]) end\n\
             return v",
    )
});

/// Atomically fetch+delete a secret-keyed Redis value (single-use).
pub async fn take_secret_key<T: serde::de::DeserializeOwned>(
    state: &AppState,
    domain: &str,
    secret: &str,
) -> Result<Option<T>> {
    let mut conn = state.redis.conn().await?;
    let key = state.redis.key(&format!(
        "{domain}:{}",
        crate::http::middleware::auth::token_hash(secret)
    ));
    let raw: Option<String> = GETDEL_SCRIPT
        .clone()
        .key(&key)
        .invoke_async(&mut *conn)
        .await?;
    raw.map(|s| {
        serde_json::from_str(&s)
            .map_err(|e| crate::Error::Internal(format!("deserialize redis value: {e}")))
    })
    .transpose()
}

/// Peek at a secret-keyed Redis value without consuming it.
pub async fn get_secret_key<T: serde::de::DeserializeOwned>(
    state: &AppState,
    domain: &str,
    secret: &str,
) -> Result<Option<T>> {
    let mut conn = state.redis.conn().await?;
    let key = state.redis.key(&format!(
        "{domain}:{}",
        crate::http::middleware::auth::token_hash(secret)
    ));
    let raw: Option<String> = conn.get(key).await?;
    raw.map(|s| {
        serde_json::from_str(&s)
            .map_err(|e| crate::Error::Internal(format!("deserialize redis value: {e}")))
    })
    .transpose()
}

/// `SET KEEPTTL` needs Redis 6.2 — this Lua equivalent works on all
/// supported versions.
pub async fn set_keep_ttl(
    conn: &mut bb8::PooledConnection<'_, bb8_redis::RedisConnectionManager>,
    key: &str,
    value: &str,
) -> Result<()> {
    static SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
        redis::Script::new(
            "local ttl = redis.call('PTTL', KEYS[1])\n\
             if ttl < 0 then ttl = 1 end\n\
             redis.call('SET', KEYS[1], ARGV[1], 'PX', ttl)\n\
             return 1",
        )
    });
    SCRIPT
        .clone()
        .key(key)
        .arg(value)
        .invoke_async::<()>(&mut **conn)
        .await?;
    Ok(())
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// User-managed API tokens (`/api/v1/tokens`)
// ---------------------------------------------------------------------------

/// `POST /api/v1/tokens` — issue a named user token whose scopes must be
/// a subset of the caller's token scopes. Returns the row id and the
/// plaintext token (shown once).
pub async fn create_user_token(
    state: &AppState,
    user_id: i64,
    name: &str,
    scopes: &str,
    expires_in_seconds: Option<u64>,
    caller_scopes: &str,
) -> Result<(i64, String)> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(crate::Error::Validation(
            "token name is required (max 200 chars)".into(),
        ));
    }
    let parsed = crate::auth::scopes::ScopeSet::parse(scopes)?;
    if parsed.0.is_empty() {
        return Err(crate::Error::Validation("scopes are required".into()));
    }
    let caller = crate::auth::scopes::ScopeSet::parse(caller_scopes)?;
    if !parsed.is_subset_of(&caller) {
        return Err(crate::Error::Validation(
            "token scopes must be a subset of the current token's scopes".into(),
        ));
    }
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM oauth_access_tokens \
         WHERE user_id = $1 AND name = $2 AND revoked_at IS NULL)",
    )
    .bind(user_id)
    .bind(name)
    .fetch_one(&state.db)
    .await?;
    if conflict {
        return Err(crate::Error::Conflict(
            "a token with that name already exists".into(),
        ));
    }
    let raw = random_secret();
    let hash = crate::http::middleware::auth::token_hash(&raw);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_access_tokens \
         (user_id, token_hash, scopes, name, expires_at) \
         VALUES ($1, $2, $3, $4, \
                 CASE WHEN $5::bigint IS NULL THEN NULL \
                      ELSE now() + $5::bigint * interval '1 second' END) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(&hash)
    .bind(parsed.as_string())
    .bind(name)
    .bind(expires_in_seconds.map(|s| s as i64))
    .fetch_one(&state.db)
    .await?;
    Ok((id, raw))
}

/// `GET /api/v1/tokens` — the user's active tokens (hashes never leave
/// the database).
pub async fn list_user_tokens(pool: &sqlx::PgPool, user_id: i64) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, name, scopes, created_at, expires_at \
         FROM oauth_access_tokens \
         WHERE user_id = $1 AND name IS NOT NULL AND revoked_at IS NULL \
         ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id").to_string(),
                "name": r.get::<Option<String>, _>("name"),
                "scopes": r.get::<String, _>("scopes").split_whitespace().collect::<Vec<_>>(),
                "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
                "expires_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at").map(|t| t.to_rfc3339()),
            })
        })
        .collect())
}

/// `DELETE /api/v1/tokens/{id}` — revoke the token (and any refresh
/// tokens chained to it).
pub async fn revoke_user_token(pool: &sqlx::PgPool, user_id: i64, token_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE oauth_refresh_tokens SET revoked_at = now() \
         WHERE access_token_id = $1",
    )
    .bind(token_id)
    .execute(&mut *tx)
    .await?;
    let changed = sqlx::query(
        "UPDATE oauth_access_tokens SET revoked_at = now() \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(token_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    tx.commit().await?;
    Ok(changed)
}
