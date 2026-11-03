//! Web Push delivery — the `WebPushSend` job enqueued by the
//! notification fanout (`epic 16` + this one).
//!
//! One job carries `{"notification_id", "account_id"}` and delivers to
//! every `web_push_subscriptions` row the recipient owns, honoring the
//! per-subscription `data.alerts` map (absent key defaults to on,
//! matching `push::create`'s all-true default) and `data.policy`
//! (`all`/`followed`/`follower`/`none` — Mastodon semantics).
//!
//! Wire format: RFC 8291 `aes128gcm` content encoding with RFC 8292
//! VAPID auth (`Authorization: vapid t=<jwt>, k=<public_key>`). The
//! payload shape mirrors Mastodon's (`notification_id`,
//! `notification_type`, `title`, `icon`) plus a `data.url` the service
//! worker navigates to on click. `access_token` is deliberately
//! absent — tokens are stored hashed (`token_hash`), so there is no
//! plaintext to embed; third-party clients fetch with the token they
//! already hold.
//!
//! Dead subscriptions (push service 400/401/403/404/410) are deleted
//! so retries stop; transient failures log a warning and the job
//! still succeeds — a slow push service must not redeliver to the
//! healthy endpoints, and the durable notification row is the real
//! state anyway.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes128Gcm, Nonce,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hkdf::Hkdf;
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    elliptic_curve::{sec1::ToSec1Point, Generate},
    PublicKey, SecretKey,
};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::Row;

use crate::{
    jobs::{JobContext, JobHandler},
    Error, Result,
};

/// The fan-out delivery job.
pub struct WebPushJob;

/// Record size declared in the `aes128gcm` header — comfortably above
/// any notification payload.
const RECORD_SIZE: u32 = 4096;

/// VAPID `exp` — 12h out, well inside the 24h cap push services
/// enforce.
const VAPID_EXP_SECONDS: i64 = 12 * 3600;

/// Push-service statuses that mean "this endpoint is dead" — delete
/// the subscription rather than retry forever.
const DEAD_STATUSES: &[u16] = &[400, 401, 403, 404, 410];

struct Subscription {
    id: i64,
    endpoint: String,
    p256dh: String,
    auth: String,
    data: Value,
}

#[async_trait::async_trait]
impl JobHandler for WebPushJob {
    async fn run(&self, ctx: &JobContext, payload: &Value) -> Result<()> {
        let (Some(notification_id), Some(account_id)) = (
            payload.get("notification_id").and_then(Value::as_i64),
            payload.get("account_id").and_then(Value::as_i64),
        ) else {
            return Ok(());
        };
        let config = &ctx.config;
        let (Some(private_key), Some(public_key), Some(subject)) = (
            config.http.vapid_private_key.as_deref(),
            config.http.vapid_public_key.as_deref(),
            config.http.webpush_subject.as_deref(),
        ) else {
            // Not configured is a steady state, not an error — retrying
            // won't mint keys.
            tracing::warn!("webpush: VAPID keys/subject not configured, skipping delivery");
            return Ok(());
        };
        let Some(row) = super::get(&ctx.db, account_id, notification_id).await? else {
            return Ok(());
        };
        let subs = subscriptions(&ctx.db, account_id).await?;
        if subs.is_empty() {
            return Ok(());
        }
        let payload = build_payload(ctx, &row).await?;
        let client = http_client(ctx);
        let mut followed: Option<bool> = None;
        let mut follower: Option<bool> = None;
        for sub in subs {
            if !alert_enabled(&sub.data, row.kind) {
                continue;
            }
            match policy_allowed(&ctx.db, &sub.data, &row, &mut followed, &mut follower).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(notification_id = row.id, error = %e, "webpush: policy check failed");
                    continue;
                }
            }
            deliver(
                ctx,
                &client,
                &sub,
                &payload,
                private_key,
                public_key,
                subject,
            )
            .await;
        }
        Ok(())
    }
}

async fn subscriptions(db: &sqlx::PgPool, account_id: i64) -> Result<Vec<Subscription>> {
    sqlx::query(
        "SELECT id, endpoint, p256dh, auth, data FROM web_push_subscriptions \
         WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| Subscription {
                id: r.get("id"),
                endpoint: r.get("endpoint"),
                p256dh: r.get("p256dh"),
                auth: r.get("auth"),
                data: r.get("data"),
            })
            .collect()
    })
    .map_err(Into::into)
}

/// `data.alerts` for this notification type — absent key defaults to
/// on (the `push::create` all-true default). `emoji_reaction` also
/// honors Moshidon's `reaction` key (the wire alias the subscription
/// API validates against).
fn alert_enabled(data: &Value, kind: super::NotificationType) -> bool {
    let alerts = data.get("alerts");
    let key = alerts.and_then(|a| a.get(kind.as_str())).or_else(|| {
        (kind == super::NotificationType::EmojiReaction)
            .then(|| alerts.and_then(|a| a.get("reaction")))
            .flatten()
    });
    key.and_then(Value::as_bool).unwrap_or(true)
}

/// `data.policy` — `none` never delivers, `followed`/`follower` check
/// the `follows` table (each direction computed lazily, once per job).
async fn policy_allowed(
    db: &sqlx::PgPool,
    data: &Value,
    row: &super::NotificationRow,
    followed: &mut Option<bool>,
    follower: &mut Option<bool>,
) -> Result<bool> {
    let policy = data.get("policy").and_then(Value::as_str).unwrap_or("all");
    match policy {
        "none" => Ok(false),
        "followed" => {
            if followed.is_none() {
                *followed = Some(
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(SELECT 1 FROM follows \
                         WHERE account_id = $1 AND target_account_id = $2)",
                    )
                    .bind(row.account_id)
                    .bind(row.from_account_id)
                    .fetch_one(db)
                    .await?,
                );
            }
            Ok(followed.unwrap_or(false))
        }
        "follower" => {
            if follower.is_none() {
                *follower = Some(
                    sqlx::query_scalar::<_, bool>(
                        "SELECT EXISTS(SELECT 1 FROM follows \
                         WHERE account_id = $1 AND target_account_id = $2)",
                    )
                    .bind(row.from_account_id)
                    .bind(row.account_id)
                    .fetch_one(db)
                    .await?,
                );
            }
            Ok(follower.unwrap_or(false))
        }
        _ => Ok(true),
    }
}

/// One-line event text per notification type — post content is never
/// included: push services and lock screens shouldn't carry DM text.
fn event_text(kind: super::NotificationType, actor: &str) -> String {
    match kind {
        super::NotificationType::Mention => format!("{actor} mentioned you"),
        super::NotificationType::Favourite => format!("{actor} favourited your post"),
        super::NotificationType::Reblog => format!("{actor} boosted your post"),
        super::NotificationType::Quote => format!("{actor} quoted your post"),
        super::NotificationType::Follow => format!("{actor} followed you"),
        super::NotificationType::FollowRequest => {
            format!("{actor} requested to follow you")
        }
        super::NotificationType::Poll => "A poll you voted in has ended".to_string(),
        super::NotificationType::Status => format!("{actor} just posted"),
        super::NotificationType::Update => format!("{actor} edited a post"),
        super::NotificationType::EmojiReaction => format!("{actor} reacted to your post"),
        super::NotificationType::AdminSignUp => format!("New sign-up: {actor}"),
        super::NotificationType::ModerationWarning => {
            "You received a moderation warning".to_string()
        }
        super::NotificationType::AdminReport => "A new report was filed".to_string(),
        super::NotificationType::Move => format!("{actor} moved to a new account"),
    }
}

/// The encrypted JSON document the service worker receives.
async fn build_payload(ctx: &JobContext, row: &super::NotificationRow) -> Result<Value> {
    let (actor_name, icon) = sender_identity(ctx, row.from_account_id).await;
    let locale: Option<String> =
        sqlx::query_scalar("SELECT locale FROM users WHERE account_id = $1")
            .bind(row.account_id)
            .fetch_optional(&ctx.db)
            .await?
            .flatten();
    Ok(json!({
        "notification_id": row.id.to_string(),
        "notification_type": row.kind.as_str(),
        "title": event_text(row.kind, &actor_name),
        "body": "",
        "icon": icon,
        "badge": "/icons/maskable-192.png",
        "tag": format!("yonder-notification-{}", row.id),
        "preferred_locale": locale.unwrap_or_else(|| "en".to_string()),
        "data": {
            "url": row
                .status_id
                .map(|id| format!("/statuses/{id}"))
                .unwrap_or_else(|| "/notifications".to_string()),
        },
    }))
}

/// Sender display label + avatar URL for the payload — falls back to
/// a neutral label and the instance icon when the account row is gone.
async fn sender_identity(ctx: &JobContext, from_account_id: i64) -> (String, String) {
    let row = sqlx::query(
        "SELECT username, domain, display_name, avatar_path, avatar_remote_url \
         FROM accounts WHERE id = $1",
    )
    .bind(from_account_id)
    .fetch_optional(&ctx.db)
    .await;
    let fallback_icon = "/icons/logo-512.png".to_string();
    let Ok(Some(r)) = row else {
        return ("Someone".to_string(), fallback_icon);
    };
    let username: String = r.get("username");
    let domain: Option<String> = r.get("domain");
    let display: Option<String> = r.get("display_name");
    let name = display
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| match &domain {
            Some(d) => format!("{username}@{d}"),
            None => username,
        });
    let avatar_path: Option<String> = r.get("avatar_path");
    let remote: Option<String> = r.get("avatar_remote_url");
    let base_url = crate::activitypub::federation_base_url(&ctx.config);
    let local = avatar_path.map(|k| crate::media::public_url(&ctx.config, &base_url, &k));
    // Same precedence as `users::account::media_url`: with remote-media
    // caching on a cached copy wins; off, the remote URL serves directly.
    let icon = if ctx.config.media.fetch_remote_media {
        local.or(remote)
    } else {
        remote.or(local)
    }
    .filter(|u| !u.is_empty())
    .unwrap_or(fallback_icon);
    (name, icon)
}

/// The pubby HTTP client — push endpoints are public push services,
/// so the SSRF guard applies unless the admin opted into private
/// targets (dev/test).
fn http_client(ctx: &JobContext) -> pubby_sdk::http::HttpClient {
    let timeout = std::time::Duration::from_secs(ctx.config.federation.request_timeout_seconds);
    if ctx.config.federation.allow_private_targets {
        pubby_sdk::http::HttpClient::new_relaxed(timeout)
    } else {
        pubby_sdk::http::HttpClient::new(timeout)
    }
}

/// Encrypt + POST to one subscription. Dead endpoints are deleted;
/// every failure logs and returns — per-subscription errors never
/// fail the job (see the module doc).
async fn deliver(
    ctx: &JobContext,
    client: &pubby_sdk::http::HttpClient,
    sub: &Subscription,
    payload: &Value,
    private_key: &str,
    public_key: &str,
    subject: &str,
) {
    let endpoint = sub.endpoint.clone();
    let p256dh = sub.p256dh.clone();
    let auth = sub.auth.clone();
    let body = match serde_json::to_vec(payload)
        .map_err(|e| Error::Internal(format!("push payload encode failed: {e}")))
        .and_then(|plain| encrypt(&p256dh, &auth, &plain))
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(subscription_id = sub.id, error = %e, "webpush: encryption failed");
            return;
        }
    };
    let jwt = match vapid_jwt(&endpoint, subject, private_key) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(subscription_id = sub.id, error = %e, "webpush: VAPID signing failed");
            return;
        }
    };
    let mut headers = std::collections::HashMap::new();
    headers.insert("TTL".to_string(), "86400".to_string());
    headers.insert("Content-Encoding".to_string(), "aes128gcm".to_string());
    headers.insert(
        "Content-Type".to_string(),
        "application/octet-stream".to_string(),
    );
    headers.insert(
        "Authorization".to_string(),
        format!("vapid t={jwt}, k={public_key}"),
    );
    let result = {
        let client = client.clone();
        let endpoint = endpoint.clone();
        tokio::task::spawn_blocking(move || client.post(&endpoint, &headers, &body)).await
    };
    match result {
        Ok(Ok(resp)) if resp.is_success() => {}
        Ok(Ok(resp)) if DEAD_STATUSES.contains(&resp.status) => {
            tracing::info!(
                subscription_id = sub.id,
                status = resp.status,
                "webpush: subscription no longer valid, removing"
            );
            if let Err(e) = sqlx::query("DELETE FROM web_push_subscriptions WHERE id = $1")
                .bind(sub.id)
                .execute(&ctx.db)
                .await
            {
                tracing::warn!(subscription_id = sub.id, error = %e, "webpush: stale delete failed");
            }
        }
        Ok(Ok(resp)) => {
            tracing::warn!(
                subscription_id = sub.id,
                status = resp.status,
                "webpush: push service rejected delivery"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(subscription_id = sub.id, error = %e, "webpush: delivery failed");
        }
        Err(e) => {
            tracing::warn!(subscription_id = sub.id, error = %e, "webpush: delivery task failed");
        }
    }
}

/// RFC 8292 §3 — an ES256 JWT with `aud` = the push service's origin,
/// `exp` ≤24h out and `sub` = the configured contact URI.
fn vapid_jwt(endpoint: &str, subject: &str, private_key_b64: &str) -> Result<String> {
    let key_bytes = URL_SAFE_NO_PAD
        .decode(private_key_b64)
        .map_err(|e| Error::Internal(format!("invalid VAPID private key: {e}")))?;
    let signing_key = SigningKey::from_slice(&key_bytes)
        .map_err(|e| Error::Internal(format!("invalid VAPID private key: {e}")))?;
    let aud = url::Url::parse(endpoint)
        .map(|u| u.origin().ascii_serialization())
        .map_err(|e| Error::Internal(format!("invalid push endpoint: {e}")))?;
    let exp = (chrono::Utc::now() + chrono::Duration::seconds(VAPID_EXP_SECONDS)).timestamp();
    let b64 = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap_or_default());
    let header = b64(&json!({"typ": "JWT", "alg": "ES256"}));
    let claims = b64(&json!({"aud": aud, "exp": exp, "sub": subject}));
    let message = format!("{header}.{claims}");
    let signature: Signature = signing_key.sign(message.as_bytes());
    Ok(format!(
        "{message}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// RFC 8291 §3.3 + RFC 8188 `aes128gcm` — ECDH with the subscriber's
/// `p256dh`, auth-secret-bound IKM, single-record ciphertext carrying
/// a 16-byte salt, the record size and the ephemeral public key.
fn encrypt(p256dh_b64: &str, auth_b64: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let decode = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|e| Error::Internal(format!("invalid subscription key material: {e}")))
    };
    let ua_public_bytes = decode(p256dh_b64)?;
    let auth_secret = decode(auth_b64)?;
    let ua_public = PublicKey::from_sec1_bytes(&ua_public_bytes)
        .map_err(|e| Error::Internal(format!("invalid p256dh key: {e}")))?;

    let ephemeral = SecretKey::generate();
    let as_public_bytes = ephemeral.public_key().to_sec1_point(false);
    let shared = p256::ecdh::diffie_hellman(ephemeral.to_nonzero_scalar(), ua_public.as_affine());

    // IKM binds the ECDH secret to the subscription's auth secret and
    // both public keys (RFC 8291 §3.3).
    let mut key_info = b"WebPush: info\x00".to_vec();
    key_info.extend_from_slice(&ua_public_bytes);
    key_info.extend_from_slice(as_public_bytes.as_bytes());
    let ikm_hkdf = Hkdf::<Sha256>::new(Some(&auth_secret), shared.raw_secret_bytes());
    let mut ikm = [0u8; 32];
    ikm_hkdf
        .expand(&key_info, &mut ikm)
        .map_err(|e| Error::Internal(format!("IKM expand failed: {e}")))?;

    let salt = uuid::Uuid::new_v4().into_bytes();
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
        .map_err(|e| Error::Internal(format!("CEK expand failed: {e}")))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\x00", &mut nonce)
        .map_err(|e| Error::Internal(format!("nonce expand failed: {e}")))?;

    // RFC 8188: the last (here: only) record ends with a 0x02
    // delimiter octet.
    let mut plain = plaintext.to_vec();
    plain.push(0x02);
    let cipher = Aes128Gcm::new_from_slice(&cek)
        .map_err(|e| Error::Internal(format!("CEK init failed: {e}")))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_ref())
        .map_err(|e| Error::Internal(format!("payload encryption failed: {e}")))?;

    let mut body =
        Vec::with_capacity(16 + 4 + 1 + as_public_bytes.as_bytes().len() + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public_bytes.as_bytes().len() as u8);
    body.extend_from_slice(as_public_bytes.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `admin::webpush::generate` output round-trips through the
    /// signer and produces a verifiable ES256 JWT.
    #[test]
    fn vapid_jwt_signs_and_verifies() {
        use p256::ecdsa::{signature::Verifier, VerifyingKey};
        let (public, private) = crate::admin::webpush::generate();
        let jwt = vapid_jwt(
            "https://push.example.com/send/abc",
            "mailto:admin@example.com",
            &private,
        )
        .unwrap();
        let (message, sig_b64) = jwt.rsplit_once('.').unwrap();
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(sig_b64).unwrap()).unwrap();
        let key_bytes = URL_SAFE_NO_PAD.decode(&public).unwrap();
        let verifying = VerifyingKey::from_sec1_bytes(&key_bytes).unwrap();
        verifying.verify(message.as_bytes(), &signature).unwrap();
        // `aud` is the endpoint origin, not the full URL.
        let claims_b64 = message.split('.').nth(1).unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims_b64).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.com");
        assert_eq!(claims["sub"], "mailto:admin@example.com");
        assert!(claims["exp"].as_i64().unwrap() > chrono::Utc::now().timestamp());
    }

    /// The `aes128gcm` body carries salt | rs | keyid-len | keyid |
    /// ciphertext, and the ciphertext decrypts when the receiver runs
    /// the same ECDH derivation from its private key.
    #[test]
    fn encrypt_produces_decryptable_aes128gcm() {
        use aes_gcm::aead::Aead;
        use p256::ecdh;
        // A simulated subscriber keypair.
        let ua_secret = SecretKey::generate();
        let ua_public_bytes = ua_secret.public_key().to_sec1_point(false);
        let p256dh = URL_SAFE_NO_PAD.encode(ua_public_bytes.as_bytes());
        let auth = URL_SAFE_NO_PAD.encode(b"0123456789abcdef");
        let plaintext = b"{\"hello\":\"push\"}";

        let body = encrypt(&p256dh, &auth, plaintext).unwrap();
        // salt | rs | idlen | keyid | ciphertext
        let salt = &body[..16];
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert_eq!(rs, RECORD_SIZE);
        let idlen = body[20] as usize;
        assert_eq!(idlen, 65);
        let as_public_bytes = &body[21..21 + idlen];
        let ciphertext = &body[21 + idlen..];
        assert_eq!(ciphertext.len(), plaintext.len() + 1 + 16);

        // Receiver side: same derivation from its private key.
        let as_public = PublicKey::from_sec1_bytes(as_public_bytes).unwrap();
        let shared = ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());
        let mut key_info = b"WebPush: info\x00".to_vec();
        key_info.extend_from_slice(ua_public_bytes.as_bytes());
        key_info.extend_from_slice(as_public_bytes);
        let ikm_hkdf = Hkdf::<Sha256>::new(Some(b"0123456789abcdef"), shared.raw_secret_bytes());
        let mut ikm = [0u8; 32];
        ikm_hkdf.expand(&key_info, &mut ikm).unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        prk.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        prk.expand(b"Content-Encoding: nonce\x00", &mut nonce)
            .unwrap();
        let cipher = Aes128Gcm::new_from_slice(&cek).unwrap();
        let decrypted = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .unwrap();
        assert_eq!(&decrypted[..decrypted.len() - 1], plaintext);
        assert_eq!(*decrypted.last().unwrap(), 0x02);
    }

    #[test]
    fn alert_map_defaults_on_and_honors_reaction_alias() {
        let data = json!({"alerts": {"favourite": false, "reaction": false}});
        assert!(alert_enabled(
            &data,
            super::super::NotificationType::Mention
        ));
        assert!(!alert_enabled(
            &data,
            super::super::NotificationType::Favourite
        ));
        assert!(!alert_enabled(
            &data,
            super::super::NotificationType::EmojiReaction
        ));
        assert!(alert_enabled(
            &json!({}),
            super::super::NotificationType::EmojiReaction
        ));
    }
}
