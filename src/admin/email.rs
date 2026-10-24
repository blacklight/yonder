//! Outbound email (epic 22): the `lettre` SMTP transport, admin-editable
//! templates (`email_templates`, migration `0021`) and the real
//! [`EmailSendJob`] handler, which replaces the epic-03 stub.
//!
//! Payload contract — two shapes are already produced by callers:
//!
//! - Pre-rendered: `{kind, to, subject, text_body, html_body?}` —
//!   sent as-is (`auth::registration` produces `verify_email` this
//!   way; the payload carries the one-time token, so it is **never**
//!   logged).
//! - Template: `{kind, account_id, vars…}` — the recipient's
//!   `users.email` is resolved by `account_id`, the
//!   `email_templates` row for `kind` is rendered with the payload's
//!   remaining fields plus `instance_name`/`user_handle`
//!   (`notifications::fanout` produces this for notification mail).
//!
//! A missing template or recipient logs a warning and acks — mail is
//! best-effort, never a dead-letter reason.

use std::sync::{Arc, OnceLock, RwLock};

use lettre::{
    message::{Mailbox, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use serde_json::{Map, Value};
use sqlx::PgPool;

use crate::{
    config::EmailConfig,
    jobs::{JobContext, JobHandler, Registry},
    Error, Result,
};

/// The admin-editable template kinds (seeded by migration `0021`).
pub const EDITABLE_KINDS: &[&str] = &[
    "verify_email",
    "password_reset",
    "welcome",
    "account_status",
];

/// What `send` needs from an implementation — the seam tests inject a
/// recording transport through.
#[async_trait::async_trait]
pub trait EmailTransport: Send + Sync {
    /// Deliver one message.
    async fn send(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        text: &str,
        html: Option<&str>,
    ) -> Result<()>;
}

/// The production transport over `lettre` SMTP. Owns its `EmailConfig`
/// so it fits the `'static` trait-object slot. The client is built
/// lazily and reused — it owns the connection pool.
pub struct LettreTransport {
    config: EmailConfig,
    client: OnceLock<std::result::Result<AsyncSmtpTransport<Tokio1Executor>, String>>,
}

impl LettreTransport {
    pub fn new(config: &EmailConfig) -> Self {
        LettreTransport {
            config: config.clone(),
            client: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&AsyncSmtpTransport<Tokio1Executor>> {
        self.client
            .get_or_init(|| build_client(&self.config).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| Error::Internal(e.clone()))
    }
}

/// Build the SMTP client. Errors render as [`Error`] and are cached
/// per transport, so a misconfigured host fails every send the same
/// way without rebuilding.
fn build_client(config: &EmailConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let host = config
        .smtp_host
        .as_deref()
        .ok_or_else(|| Error::Validation("mail server is not configured".into()))?;
    let mut builder = if config.smtp_tls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
            .map_err(|e| Error::Internal(format!("smtp transport config: {e}")))?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
    }
    .port(config.smtp_port);
    if let Some(user) = &config.smtp_username {
        if !config.smtp_tls {
            tracing::warn!("smtp_tls is off — SMTP credentials will cross the wire unencrypted");
        }
        let pass = config.smtp_password.clone().unwrap_or_default();
        builder = builder.credentials(Credentials::new(user.clone(), pass));
    }
    Ok(builder.build())
}

#[async_trait::async_trait]
impl EmailTransport for LettreTransport {
    async fn send(
        &self,
        from: &str,
        to: &str,
        subject: &str,
        text: &str,
        html: Option<&str>,
    ) -> Result<()> {
        let parse = |addr: &str| -> Result<Mailbox> {
            addr.parse()
                .map_err(|_| Error::Validation(format!("invalid mail address {addr:?}")))
        };
        let builder = Message::builder()
            .from(parse(from)?)
            .to(parse(to)?)
            .subject(subject);
        let message = match html {
            // Both parts — HTML-only mail leaves clients that don't
            // render it without a body.
            Some(html) => builder
                .multipart(
                    MultiPart::alternative()
                        .singlepart(SinglePart::plain(text.to_string()))
                        .singlepart(SinglePart::html(html.to_string())),
                )
                .map_err(|e| Error::Internal(format!("build message: {e}")))?,
            None => builder
                .singlepart(SinglePart::plain(text.to_string()))
                .map_err(|e| Error::Internal(format!("build message: {e}")))?,
        };
        self.client()?
            .send(message)
            .await
            .map_err(|e| Error::Internal(format!("smtp send: {e}")))?;
        Ok(())
    }
}

/// Test seam — integration tests inject a recording transport so no
/// SMTP container is needed.
static TEST_TRANSPORT: OnceLock<Arc<dyn EmailTransport>> = OnceLock::new();

/// Inject the transport used by every `send`. Test-only: persists for
/// the process.
#[doc(hidden)]
pub fn set_test_transport(transport: Arc<dyn EmailTransport>) {
    let _ = TEST_TRANSPORT.set(transport);
}

/// The configured transport — the injected test transport wins.
/// Otherwise the production transport is cached per config so its
/// connection pool survives across sends; an `email.*` overlay change
/// builds a fresh transport on the next send.
static SMTP_TRANSPORT: RwLock<Option<(EmailConfig, Arc<dyn EmailTransport>)>> = RwLock::new(None);

fn transport(config: &EmailConfig) -> Arc<dyn EmailTransport> {
    if let Some(t) = TEST_TRANSPORT.get() {
        return t.clone();
    }
    if let Some((cached_config, t)) = &*SMTP_TRANSPORT.read().expect("smtp cache poisoned") {
        if cached_config == config {
            return t.clone();
        }
    }
    let t: Arc<dyn EmailTransport> = Arc::new(LettreTransport::new(config));
    *SMTP_TRANSPORT.write().expect("smtp cache poisoned") = Some((config.clone(), t.clone()));
    t
}

/// `{{var}}` substitution — unknown vars render as empty; values
/// stringify (`"x"`, `42`, `true`). `escape` post-processes each
/// substituted value.
fn render_with(
    template: &str,
    vars: &Map<String, Value>,
    escape: impl Fn(&str) -> String,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                if let Some(value) = vars.get(name) {
                    let raw = match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.push_str(&escape(&raw));
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Raw `{{var}}` substitution — for subjects and plain-text bodies.
pub fn render(template: &str, vars: &Map<String, Value>) -> String {
    render_with(template, vars, |s| s.to_string())
}

/// `render` for HTML bodies — substituted values are entity-escaped so
/// admin- or user-controlled strings can't inject markup. The template
/// itself is trusted (admins edit it).
pub fn render_html(template: &str, vars: &Map<String, Value>) -> String {
    render_with(template, vars, escape_html)
}

/// Minimal entity escaping for template values in HTML bodies.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether the class flag for `kind` allows sending. Unflagged kinds
/// (notification mail from `fanout`) always send — their per-user
/// preference was already checked at enqueue time.
fn class_enabled(config: &EmailConfig, kind: &str) -> bool {
    match kind {
        "verify_email" => config.send_verification,
        "password_reset" => config.send_password_reset,
        "welcome" => config.send_welcome,
        "account_status" => config.send_account_status,
        _ => true,
    }
}

/// Deliver one message through the configured transport. The SMTP
/// transport errors when no `smtp_host` is configured; the `EmailSend`
/// job checks that first and skips instead, and the test endpoint
/// surfaces it as a 422.
pub async fn send_now(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<()> {
    transport(config)
        .send(&config.from_address, to, subject, text, html)
        .await
}

/// Load one template row.
pub async fn template(
    pool: &PgPool,
    kind: &str,
) -> Result<Option<(String, String, Option<String>)>> {
    sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT subject, text_body, html_body FROM email_templates WHERE kind = $1",
    )
    .bind(kind)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

/// Template vars for a template-shaped payload: every payload field
/// except the envelope keys becomes a var, plus `instance_name` and
/// `user_handle`.
fn vars_for(payload: &Value, instance_name: &str, user_handle: &str) -> Map<String, Value> {
    const ENVELOPE: &[&str] = &[
        "kind",
        "account_id",
        "to",
        "subject",
        "text_body",
        "html_body",
        "force",
    ];
    let mut vars = Map::new();
    if let Value::Object(map) = payload {
        for (key, value) in map {
            if !ENVELOPE.contains(&key.as_str()) {
                vars.insert(key.clone(), value.clone());
            }
        }
    }
    vars.insert(
        "instance_name".into(),
        Value::String(instance_name.to_string()),
    );
    vars.insert("user_handle".into(), Value::String(user_handle.to_string()));
    vars
}

/// `JobKind::EmailSend` — real SMTP sending.
pub struct EmailSendJob;

#[async_trait::async_trait]
impl JobHandler for EmailSendJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        // The payload may carry bearer-grade secrets — never log it.
        let kind = payload
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let email = &ctx.config.email;
        // `force` bypasses the class flag — admin-triggered reset mail
        // (`account.email_reset`) is an explicit send, not a class.
        let forced = payload
            .get("force")
            .is_some_and(|v| v.as_bool() == Some(true));
        if !forced && !class_enabled(email, kind) {
            return Ok(());
        }
        if email.smtp_host.is_none() {
            tracing::warn!(
                kind,
                "mail class enabled but no smtp_host configured — skipping"
            );
            return Ok(());
        }

        if let Some(text) = payload.get("text_body").and_then(Value::as_str) {
            // Pre-rendered shape: the caller did the work.
            let Some(to) = payload.get("to").and_then(Value::as_str) else {
                tracing::warn!(kind, "email job with text_body but no to — skipping");
                return Ok(());
            };
            let subject = payload
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("Yonder");
            let html = payload.get("html_body").and_then(Value::as_str);
            return send_now(email, to, subject, text, html).await;
        }

        // Template shape: resolve the recipient and render.
        let Some(account_id) = payload.get("account_id").and_then(Value::as_i64) else {
            tracing::warn!(kind, "email job with no recipient — skipping");
            return Ok(());
        };
        let recipient: Option<(Option<String>, String)> = sqlx::query_as(
            "SELECT u.email, a.username FROM users u \
             JOIN accounts a ON a.id = u.account_id WHERE u.account_id = $1",
        )
        .bind(account_id)
        .fetch_optional(&ctx.db)
        .await?;
        let Some((Some(to), user_handle)) = recipient else {
            tracing::warn!(kind, account_id, "email job: no local recipient — skipping");
            return Ok(());
        };
        let Some((subject, text, html)) = template(&ctx.db, kind).await? else {
            tracing::warn!(kind, "email job: no template for kind — skipping");
            return Ok(());
        };
        let vars = vars_for(payload, &ctx.config.instance.name, &user_handle);
        let html_rendered = html.as_deref().map(|h| render_html(h, &vars));
        send_now(
            email,
            &to,
            &render(&subject, &vars),
            &render(&text, &vars),
            html_rendered.as_deref(),
        )
        .await
    }
}

/// Register the real sender, replacing the epic-03 stub. Call after
/// `jobs::stub::register_all` (the registry is a HashMap — last write
/// wins).
pub fn register_jobs(registry: &mut Registry) {
    registry.register(crate::jobs::JobKind::EmailSend, EmailSendJob);
}
