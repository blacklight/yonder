//! Registration: username validation, registration-mode policy, and the
//! account → user → ActivityPub-actor transaction.

use serde::Deserialize;

use crate::{
    activitypub::{self, actors::provision_local_conn},
    auth::{
        password::{hash_password, MIN_PASSWORD_LENGTH},
        scopes::requires_email_verification,
        tokens::{self, random_secret},
    },
    config::RegistrationMode,
    http::AppState,
    jobs::JobKind,
    Error, Result,
};

/// Local usernames that must never be registered — the instance actor
/// owns `yonder.instance`.
const RESERVED_USERNAMES: &[&str] = &["yonder.instance", "instance"];

/// Redis TTL for email-verification tokens.
const EMAIL_VERIFY_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Validated `POST /api/v1/accounts` input.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RegisterParams {
    pub username: String,
    pub email: String,
    pub password: String,
    pub agreement: Option<bool>,
    pub locale: Option<String>,
    /// Approval-required registration reason.
    pub reason: Option<String>,
}

/// What a successful [`register`] produced.
#[derive(Debug)]
pub struct Registered {
    pub user_id: i64,
    pub account_id: i64,
    /// `true` when an email-verification token was issued (the account
    /// is unverified until `verify_email` consumes it).
    pub verification_pending: bool,
}

/// Validate a candidate username per ActivityPub rules: `[a-zA-Z0-9_]`
/// only (no hyphens), length ≤ `content.max_username_length`, not
/// reserved (case-insensitive).
pub fn validate_username(config: &crate::config::Config, username: &str) -> Result<()> {
    let max = config.content.max_username_length as usize;
    if username.is_empty() || username.chars().count() > max {
        return Err(Error::Validation(format!(
            "username must be 1–{max} characters"
        )));
    }
    // Reserved names are checked before the charset: `yonder.instance`
    // can't pass the charset anyway (`.`), but the reserved error is
    // the honest one — and keeps the guard reachable.
    if RESERVED_USERNAMES.contains(&username.to_lowercase().as_str()) {
        return Err(Error::Validation("username is reserved".to_string()));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(Error::Validation(
            "username may only contain letters, numbers and underscores".to_string(),
        ));
    }
    Ok(())
}

/// Create the local `accounts` + `users` rows, provision the
/// ActivityPub actor, and issue the caller's first token pair — one
/// transaction, so a failure anywhere (actor provisioning, token
/// insert) rolls the whole registration back rather than leaving an
/// account the client cannot log into.
///
/// The optional bearer **app token** binds the issued tokens to that
/// application and records the `oauth_authorizations` row in the same
/// transaction. Email verification (Redis token + `EmailSend` enqueue)
/// runs *before* commit: a Redis outage aborts cleanly with no account
/// row, at the cost of a stray TTL'd verify key if the commit itself
/// fails afterwards.
///
/// `accounts.actor_id` is `NOT NULL UNIQUE` — the deterministic actor
/// URL is written on insert (an empty placeholder would collide across
/// concurrent sign-ups) and `provision_local_conn` fills keys/URLs.
pub async fn register(
    state: &AppState,
    params: &RegisterParams,
    app: Option<&tokens::AppAuth>,
) -> Result<(Registered, tokens::IssuedTokens)> {
    if !params.agreement.unwrap_or(false) {
        return Err(Error::Validation(
            "agreement to the instance rules is required".to_string(),
        ));
    }
    let config = &state.config;
    let reason = params
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    // The length bound applies whenever a reason is present — the
    // column CHECK would otherwise surface an over-limit reason in
    // `open` mode as a 500.
    if reason.is_some_and(|r| {
        r.chars().count() > config.content.max_registration_message_length as usize
    }) {
        return Err(Error::Validation(format!(
            "reason is longer than {} characters",
            config.content.max_registration_message_length
        )));
    }
    match config.auth.registration_mode {
        RegistrationMode::Closed => {
            return Err(Error::Forbidden);
        }
        RegistrationMode::ApprovalRequired if reason.is_none() => {
            return Err(Error::Validation(
                "reason is required when registrations need approval".to_string(),
            ));
        }
        _ => {}
    }
    let username = params.username.trim();
    validate_username(config, username)?;
    let email = params.email.trim();
    if email.is_empty() || !email.contains('@') || email.chars().count() > 500 {
        return Err(Error::Validation("a valid email is required".to_string()));
    }
    if params.password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(Error::Validation(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    let password_hash = hash_password(&params.password)?;
    let base_url = activitypub::federation_base_url(config);
    let actor_id = format!("{base_url}/ap/users/{username}");
    let email_verified = !requires_email_verification(config);
    let approved = matches!(config.auth.registration_mode, RegistrationMode::Open);

    // Friendly pre-checks; the unique indexes still guard the race.
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL)",
    )
    .bind(username)
    .fetch_one(&state.db)
    .await?;
    if taken {
        return Err(Error::Validation("username is already taken".to_string()));
    }
    let email_taken: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE lower(email) = lower($1))")
            .bind(email)
            .fetch_one(&state.db)
            .await?;
    if email_taken {
        return Err(Error::Validation("email is already taken".to_string()));
    }

    let mut tx = state.db.begin().await?;
    let account_id: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (domain, username, actor_id) VALUES (NULL, $1, $2) RETURNING id",
    )
    .bind(username)
    .bind(&actor_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(registration_db_error)?;
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users \
         (account_id, email, password_hash, approved_at, email_verified_at, registration_reason, locale) \
         VALUES ($1, $2, $3, CASE WHEN $4 THEN now() END, CASE WHEN $5 THEN now() END, $6, $7) \
         RETURNING id",
    )
    .bind(account_id)
    .bind(email)
    .bind(&password_hash)
    .bind(approved)
    .bind(email_verified)
    .bind(reason)
    .bind(&params.locale)
    .fetch_one(&mut *tx)
    .await
    .map_err(registration_db_error)?;
    provision_local_conn(&mut tx, &base_url, account_id, username).await?;

    // The first token pair (+ the authorization record for app-bound
    // registrations) commits with the account, not after it.
    let scopes = app
        .map(|a| a.scopes.clone())
        .unwrap_or_else(|| "read write".to_string());
    let issued = tokens::issue_token_pair_conn(
        &mut tx,
        config,
        app.map(|a| a.application_id),
        user_id,
        &scopes,
        None,
        None,
    )
    .await?;
    if let Some(app) = app {
        tokens::record_authorization(&mut *tx, user_id, app.application_id, &scopes).await?;
    }
    // Redis work runs before commit — a Redis outage rolls everything
    // back so the client can retry cleanly.
    let verification_pending = !email_verified;
    if verification_pending {
        enqueue_verification_email(state, user_id, email).await?;
    }
    tx.commit().await?;

    Ok((
        Registered {
            user_id,
            account_id,
            verification_pending,
        },
        issued,
    ))
}

/// Store a verification token (Redis, sha256-keyed, single-use) and
/// enqueue the `EmailSend` job. The sender is a stub until epic 22 —
/// the payload carries the token but must never reach logs.
async fn enqueue_verification_email(state: &AppState, user_id: i64, email: &str) -> Result<()> {
    let token = random_secret();
    crate::auth::tokens::set_secret_key(
        state,
        "email-verify",
        &token,
        &serde_json::json!(user_id),
        EMAIL_VERIFY_TTL_SECONDS,
    )
    .await?;
    let base = activitypub::federation_base_url(&state.config);
    state
        .dispatcher
        .enqueue(
            JobKind::EmailSend,
            serde_json::json!({
                "kind": "verify_email",
                "to": email,
                "subject": "Verify your Yonder account",
                "text_body": format!(
                    "Verify your email address by POSTing this token to {base}/api/v1/accounts/verify_email:\n\n{token}"
                ),
            }),
        )
        .await?;
    Ok(())
}

/// `POST /api/v1/accounts/verify_email` — consume the Redis token and
/// mark the user's email verified.
pub async fn verify_email(state: &AppState, token: &str) -> Result<bool> {
    let user_id: Option<i64> =
        crate::auth::tokens::take_secret_key(state, "email-verify", token).await?;
    let Some(user_id) = user_id else {
        return Ok(false);
    };
    let updated =
        sqlx::query("UPDATE users SET email_verified_at = now(), updated_at = now() WHERE id = $1")
            .bind(user_id)
            .execute(&state.db)
            .await?
            .rows_affected()
            > 0;
    Ok(updated)
}

/// Map unique violations to friendly 422s; other DB errors pass through.
fn registration_db_error(e: sqlx::Error) -> Error {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            let msg = db.constraint().unwrap_or_default();
            if msg.contains("username") {
                return Error::Validation("username is already taken".to_string());
            }
            if msg.contains("email") {
                return Error::Validation("email is already taken".to_string());
            }
            return Error::Validation("duplicate value".to_string());
        }
    }
    Error::Database(e)
}
