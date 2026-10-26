//! `yonder user` — admin account management over the shared
//! registration/deletion/moderation services. Passwords are never
//! accepted on argv: `--password-stdin`, `--password-file` or
//! `--generate-password` (printed once, to stderr).

use std::path::Path;
use std::sync::Arc;

use clap::Subcommand;
use serde_json::{json, Value};

use crate::{
    auth::{
        password::{hash_password, MIN_PASSWORD_LENGTH},
        registration, tokens,
    },
    moderation::{actions, audit},
    users::{account, deletion},
    Error, Result,
};

use super::{confirm, context, output::Output};

/// Hard cap on password input from stdin or a file.
const PASSWORD_MAX_BYTES: usize = 4096;

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create a local account (bypasses registration mode). Exactly
    /// one password source is required.
    #[command(group(
        clap::ArgGroup::new("password").required(true).multiple(false)
    ))]
    Create {
        /// Username (letters, digits, underscores).
        #[arg(long)]
        username: String,
        /// Email address.
        #[arg(long)]
        email: String,
        /// Read the password from stdin (one line, ≤ 4096 bytes).
        #[arg(long, group = "password")]
        password_stdin: bool,
        /// Read the password from a file (≤ 4096 bytes).
        #[arg(long, group = "password", value_name = "PATH")]
        password_file: Option<String>,
        /// Generate a random password and print it to stderr once.
        #[arg(long, group = "password")]
        generate_password: bool,
        /// Account role.
        #[arg(long, default_value = "user", value_parser = ["user", "moderator", "admin"])]
        role: String,
        /// Create the account already verified, skipping the
        /// email-verification requirement (implied for
        /// moderator/admin roles).
        #[arg(long)]
        skip_verification: bool,
        /// Interface locale for the new user.
        #[arg(long)]
        locale: Option<String>,
    },
    /// Delete a local account: scheduled (grace period) by default,
    /// purged immediately with --now.
    Delete {
        /// Local username.
        username: String,
        /// Purge immediately instead of scheduling for the
        /// configured grace period.
        #[arg(long)]
        now: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List local user accounts.
    List {
        /// Only this role.
        #[arg(long, value_parser = ["user", "moderator", "admin"])]
        role: Option<String>,
        /// List suspended accounts instead of active ones.
        #[arg(long)]
        suspended: bool,
        /// Maximum rows (1–200, default 100).
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Case-insensitive substring match on username or email.
        #[arg(long, value_name = "TEXT")]
        search: Option<String>,
    },
    /// Grant (or with --revoke, remove) the admin role.
    SetAdmin {
        /// Local username.
        username: String,
        /// Demote back to a regular user.
        #[arg(long)]
        revoke: bool,
    },
    /// Grant (or with --revoke, remove) the moderator role.
    SetModerator {
        /// Local username.
        username: String,
        /// Demote back to a regular user.
        #[arg(long)]
        revoke: bool,
    },
    /// Generate a one-time password-reset link for a user. The URL is
    /// a credential — it is printed to stderr, never stdout.
    ResetPassword {
        /// Local username.
        username: String,
        /// Whether to also send the reset email.
        #[arg(long, default_value = "when_enabled",
              value_parser = ["always", "when_enabled", "never"])]
        email: String,
    },
    /// Disable a user's two-factor authentication (TOTP, WebAuthn and
    /// backup codes).
    DisableTwoFa {
        /// Local username.
        username: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Mark a user's email address as verified.
    VerifyEmail {
        /// Local username.
        username: String,
    },
    /// Force a user to reset their password on next login.
    RequireReset {
        /// Local username.
        username: String,
    },
}

pub async fn run(
    path: Option<&Path>,
    command: UserCommand,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    match command {
        UserCommand::Create {
            username,
            email,
            password_stdin,
            password_file,
            generate_password,
            role,
            skip_verification,
            locale,
        } => {
            create(
                path,
                out,
                actor,
                CreateArgs {
                    username,
                    email,
                    password_stdin,
                    password_file,
                    generate_password,
                    role,
                    skip_verification,
                    locale,
                },
            )
            .await
        }
        UserCommand::Delete { username, now, yes } => {
            delete(path, &username, now, yes, out, actor).await
        }
        UserCommand::List {
            role,
            suspended,
            limit,
            search,
        } => {
            list(
                path,
                role.as_deref(),
                suspended,
                limit,
                search.as_deref(),
                out,
            )
            .await
        }
        UserCommand::SetAdmin { username, revoke } => {
            set_role(path, &username, "admin", revoke, out, actor).await
        }
        UserCommand::SetModerator { username, revoke } => {
            set_role(path, &username, "moderator", revoke, out, actor).await
        }
        UserCommand::ResetPassword { username, email } => {
            reset_password(path, &username, &email, out, actor).await
        }
        UserCommand::DisableTwoFa { username, yes } => {
            disable_2fa(path, &username, yes, out, actor).await
        }
        UserCommand::VerifyEmail { username } => verify_email(path, &username, out, actor).await,
        UserCommand::RequireReset { username } => require_reset(path, &username, out, actor).await,
    }
}

/// Resolve an account-targeting username: local, non-deleted, with a
/// `users` row (suspended included). The instance actor is refused
/// explicitly — it can never be the target of a user command.
async fn resolve_target(pool: &sqlx::PgPool, username: &str) -> Result<account::AccountRow> {
    if username.eq_ignore_ascii_case(crate::activitypub::actors::INSTANCE_USERNAME) {
        return Err(Error::Validation(
            "the instance actor cannot be modified".to_string(),
        ));
    }
    account::load_local_admin_view(pool, username)
        .await?
        .ok_or_else(|| Error::NotFound(format!("local account {username:?}")))
}

/// The `users.id` for an account (2FA and verification rows key on
/// the user, not the account).
async fn user_id_for(pool: &sqlx::PgPool, account_id: i64) -> Result<i64> {
    sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| Error::NotFound("user".to_string()))
}

struct CreateArgs {
    username: String,
    email: String,
    password_stdin: bool,
    password_file: Option<String>,
    generate_password: bool,
    role: String,
    skip_verification: bool,
    locale: Option<String>,
}

/// Bound a password source to [`PASSWORD_MAX_BYTES`] and strip one
/// trailing line ending — nothing else, so intentional whitespace
/// inside the password survives.
fn read_password_source(args: &CreateArgs) -> Result<String> {
    use std::io::Read;
    let raw: Vec<u8> = if args.password_stdin {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .take(PASSWORD_MAX_BYTES as u64 + 1)
            .read_to_end(&mut buf)?;
        buf
    } else if let Some(path) = &args.password_file {
        let mut buf = Vec::new();
        std::fs::File::open(path)
            .map_err(Error::Io)?
            .take(PASSWORD_MAX_BYTES as u64 + 1)
            .read_to_end(&mut buf)?;
        buf
    } else {
        return Err(Error::Validation(
            "one password source is required: --password-stdin, \
             --password-file or --generate-password"
                .to_string(),
        ));
    };
    if raw.len() > PASSWORD_MAX_BYTES {
        return Err(Error::Validation(format!(
            "password input exceeds {PASSWORD_MAX_BYTES} bytes"
        )));
    }
    let mut password = String::from_utf8(raw)
        .map_err(|_| Error::Validation("password input is not valid UTF-8".to_string()))?;
    if let Some(stripped) = password.strip_suffix('\n') {
        let stripped = stripped.strip_suffix('\r').unwrap_or(stripped);
        password.truncate(stripped.len());
    }
    Ok(password)
}

async fn create(
    path: Option<&Path>,
    out: &Output,
    actor: Option<&str>,
    args: CreateArgs,
) -> Result<u8> {
    let password = if args.generate_password {
        let generated = tokens::random_secret();
        // A credential shown exactly once — stderr only, never
        // stdout, JSON, or logs.
        out.warn(&format!("generated password: {generated}"));
        generated
    } else {
        read_password_source(&args)?
    };
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(Error::Validation(format!(
            "password must be at least {MIN_PASSWORD_LENGTH} characters"
        )));
    }
    let password_hash = hash_password(&password)?;
    let ctx = context::load_runtime(path, true).await?;
    let actor_id = context::resolve_audit_actor(&ctx.pool, actor).await?;
    let registered = registration::create_admin_account(
        &ctx.state,
        &registration::AdminCreateParams {
            username: &args.username,
            email: &args.email,
            password_hash: &password_hash,
            role: &args.role,
            skip_verification: args.skip_verification,
            locale: args.locale.as_deref(),
            actor_id,
        },
    )
    .await?;
    if out.json_mode() {
        out.emit_json(&json!({
            "username": args.username,
            "account_id": registered.account_id,
            "user_id": registered.user_id,
            "email_verified": !registered.verification_pending,
        }))?;
    } else {
        out.emit_human(&format!(
            "created {} (id {})",
            args.username, registered.account_id
        ))?;
        if registered.verification_pending {
            out.emit_human(
                "note: email verification pending — the user cannot post until verified",
            )?;
        }
    }
    Ok(0)
}

async fn delete(
    path: Option<&Path>,
    username: &str,
    now: bool,
    yes: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, true).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    if !confirm::confirm(&format!("delete user {username}?"), yes)? {
        return confirm::declined(out);
    }
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    if now {
        // `--now`: no enqueue — the inline purge below is the only
        // purger, so a queued job can't race the CLI for the claim.
        deletion::schedule_account_deletion_cli(
            &ctx.state,
            target.id,
            actor.account_id,
            std::time::Duration::ZERO,
            false,
        )
        .await?;
        out.warn(&format!("purging {username} — this can take a while"));
        let job_ctx = crate::jobs::JobContext {
            db: ctx.pool.clone(),
            redis: ctx.redis.clone(),
            config: Arc::new(ctx.config.clone()),
            media: crate::media::storage::MediaStore::from_config(&ctx.config),
        };
        deletion::purge_account(&job_ctx, ctx.ap.as_ref(), target.id).await?;
        if out.json_mode() {
            out.emit_json(&json!({"account_id": target.id, "deleted": true}))?;
        } else {
            out.emit_human(&format!("deleted {username}"))?;
        }
        return Ok(0);
    }
    let grace_days = u64::from(ctx.config.admin.account_deletion_grace_days);
    let schedule = deletion::schedule_account_deletion_cli(
        &ctx.state,
        target.id,
        actor.account_id,
        std::time::Duration::from_secs(grace_days * 24 * 60 * 60),
        true,
    )
    .await?;
    if out.json_mode() {
        out.emit_json(&json!({
            "account_id": target.id,
            "scheduled": true,
            "due_at": schedule.scheduled_at.to_rfc3339(),
        }))?;
    } else {
        out.emit_human(&format!(
            "scheduled {username} for deletion at {}",
            schedule.scheduled_at.to_rfc3339()
        ))?;
    }
    Ok(0)
}

async fn list(
    path: Option<&Path>,
    role: Option<&str>,
    suspended: bool,
    limit: u32,
    search: Option<&str>,
    out: &Output,
) -> Result<u8> {
    if !(1..=200).contains(&limit) {
        return Err(Error::Validation("--limit must be in 1..=200".to_string()));
    }
    let ctx = context::load_db(path).await?;
    let rows = account::admin_list(&ctx.pool, role, suspended, search, i64::from(limit)).await?;
    if out.json_mode() {
        out.emit_json(&Value::Array(rows))?;
        return Ok(0);
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                r["account_id"].to_string(),
                r["username"].as_str().unwrap_or_default().to_string(),
                r["role"].as_str().unwrap_or_default().to_string(),
                r["email"].as_str().unwrap_or_default().to_string(),
                r["created_at"].as_str().unwrap_or_default().to_string(),
            ]
        })
        .collect();
    out.table(&["id", "username", "role", "email", "created"], &table)?;
    Ok(0)
}

async fn set_role(
    path: Option<&Path>,
    username: &str,
    role: &str,
    revoke: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let new_role = if revoke { "user" } else { role };
    actions::set_role(&ctx.state, actor.account_id, target.id, new_role).await?;
    if out.json_mode() {
        out.emit_json(&json!({
            "account_id": target.id,
            "username": username,
            "role": new_role,
        }))?;
    } else {
        out.emit_human(&format!("{username} is now {new_role}"))?;
    }
    Ok(0)
}

async fn reset_password(
    path: Option<&Path>,
    username: &str,
    email: &str,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let url =
        actions::reset_link(&ctx.state, actor.account_id, &actor.role, target.id, email).await?;
    // The reset URL is a credential: stderr only, never stdout or JSON.
    out.warn(&format!("reset link (one-time, 2h): {url}"));
    if out.json_mode() {
        out.emit_json(&json!({"reset_url_sent": true}))?;
    } else {
        out.emit_human(&format!("reset link generated for {username}"))?;
    }
    Ok(0)
}

async fn disable_2fa(
    path: Option<&Path>,
    username: &str,
    yes: bool,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    if !confirm::confirm(&format!("disable two-factor auth for {username}?"), yes)? {
        return confirm::declined(out);
    }
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let user_id = user_id_for(&ctx.pool, target.id).await?;
    let mut tx = ctx.pool.begin().await?;
    crate::auth::two_factor::admin_disable_2fa_conn(&mut tx, user_id).await?;
    audit::record(
        &mut *tx,
        Some(actor.account_id),
        "account.disable_2fa",
        "account",
        Some(target.id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    if out.json_mode() {
        out.emit_json(&json!({"account_id": target.id, "two_factor_enabled": false}))?;
    } else {
        out.emit_human(&format!("two-factor auth disabled for {username}"))?;
    }
    Ok(0)
}

async fn verify_email(
    path: Option<&Path>,
    username: &str,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    let mut tx = ctx.pool.begin().await?;
    let verified = registration::admin_verify_email(&mut tx, target.id).await?;
    audit::record(
        &mut *tx,
        Some(actor.account_id),
        "account.verify_email",
        "account",
        Some(target.id),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    if out.json_mode() {
        out.emit_json(&json!({"account_id": target.id, "verified": verified}))?;
    } else {
        out.emit_human(&format!("email verified for {username}"))?;
    }
    Ok(0)
}

async fn require_reset(
    path: Option<&Path>,
    username: &str,
    out: &Output,
    actor: Option<&str>,
) -> Result<u8> {
    let ctx = context::load_runtime(path, false).await?;
    let target = resolve_target(&ctx.pool, username).await?;
    let actor = context::resolve_actor(&ctx.pool, actor).await?;
    actions::apply(
        &ctx.state,
        actor.account_id,
        &actor.role,
        target.id,
        "require_password_reset",
        "cli",
        None,
    )
    .await?;
    if out.json_mode() {
        out.emit_json(&json!({"account_id": target.id, "require_password_reset": true}))?;
    } else {
        out.emit_human(&format!("password reset required for {username}"))?;
    }
    Ok(0)
}
