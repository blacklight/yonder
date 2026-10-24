//! Epic-08 two-factor authentication tests — RFC 6238 vectors (pure
//! unit tests) plus full TOTP/backup-code/WebAuthn lifecycle tests
//! against testcontainers Postgres+Redis via `tower::oneshot` (same
//! harness conventions as `tests/auth.rs`).
//!
//! Skipped when no container runtime is reachable.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use webauthn_authenticator_rs::{softtoken::SoftToken, WebauthnAuthenticator};
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};
use yonder::{
    config::{Config, RateLimit},
    http::{build_router, AppState},
};

use crate::harness;

struct Env {
    app: Router,
    state: Arc<AppState>,
}

async fn test_app(mut config: Config) -> Option<Env> {
    let infra = harness::infra().await?;
    let slug = harness::test_slug();
    let pool = harness::provision_db(infra, &slug).await;
    let redis_handle = harness::scoped_redis(infra, &mut config.redis, &slug).await;

    config.rate_limit.authentication = RateLimit {
        limit: 10_000,
        period_seconds: 60,
        authenticated_limit: None,
    };
    config.rate_limit.account_creation = config.rate_limit.authentication.clone();

    let state = Arc::new(AppState::new(config, pool, redis_handle));
    let app = build_router(state.clone());
    Some(Env { app, state })
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.email.smtp_host = None;
    // 2FA needs the AEAD master key — tests set it explicitly.
    config.auth.secret_key = Some("test-secret-key-material".to_string());
    config
}

async fn request(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn json_req(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn authed(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    Request::from_parts(parts, body)
}

fn form_req(path: &str, form: &[(&str, &str)]) -> Request<Body> {
    let encoded = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(encoded))
        .unwrap()
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn register_user(app: &Router, username: &str, email: &str) -> (StatusCode, Value) {
    request(
        app,
        json_req(
            "POST",
            "/api/v1/accounts",
            json!({
                "username": username, "email": email,
                "password": "hunter2password", "agreement": true,
            }),
        ),
    )
    .await
}

async fn password_login(app: &Router, email: &str, password: &str) -> (StatusCode, Value) {
    request(
        app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("username", email),
                ("password", password),
            ],
        ),
    )
    .await
}

/// The current 6-digit code for a base32 secret — what an
/// authenticator app would display.
fn current_totp(secret_b32: &str) -> String {
    let secret = totp_rs::Secret::try_from_base32(secret_b32).unwrap();
    totp_rs::Builder::new()
        .with_secret(secret.as_bytes().to_vec())
        .build()
        .unwrap()
        .generate_current()
        .to_string()
}

// ---------------------------------------------------------------------------
// RFC 6238 vectors (no containers needed)
// ---------------------------------------------------------------------------

#[test]
fn rfc6238_appendix_b_vectors() {
    // RFC 6238 Appendix B — T=59s, 8 digits. The vectors use a
    // different-length ASCII secret per algorithm (20/32/64 bytes).
    let cases = [
        (
            totp_rs::Algorithm::SHA1,
            b"12345678901234567890".to_vec(),
            "94287082",
        ),
        (
            totp_rs::Algorithm::SHA256,
            b"12345678901234567890123456789012".to_vec(),
            "46119246",
        ),
        (
            totp_rs::Algorithm::SHA512,
            b"1234567890123456789012345678901234567890123456789012345678901234".to_vec(),
            "90693936",
        ),
    ];
    for (algorithm, secret, expected) in cases {
        let totp = totp_rs::Builder::new()
            .with_algorithm(algorithm)
            .with_digits(8)
            .with_step_duration(30)
            .with_secret(secret)
            .build()
            .unwrap();
        assert_eq!(totp.generate(59).to_string(), expected, "{algorithm:?}");
    }
    // 6-digit cross-check: SHA1/6/truncation of the same step.
    let totp = totp_rs::Builder::new()
        .with_digits(6)
        .with_secret(b"12345678901234567890".to_vec())
        .build()
        .unwrap();
    assert_eq!(totp.generate(59).to_string(), "287082");
}

#[test]
fn totp_skew_window() {
    // ±1 window accepted, ±2 rejected — `check` returns the matched step.
    let totp = totp_rs::Builder::new()
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(b"12345678901234567890".to_vec())
        .build()
        .unwrap();
    let code = totp.generate(59).to_string(); // step 1
    assert_eq!(totp.check(&code, 59), Some(1)); // same step
    assert!(totp.check(&code, 89).is_some()); // next step, within skew
    assert!(totp.check(&code, 29).is_some()); // previous step, within skew
    assert!(totp.check(&code, 119).is_none()); // two steps out
    assert!(totp.check("000000", 59).is_none()); // wrong code
}

// ---------------------------------------------------------------------------
// TOTP lifecycle
// ---------------------------------------------------------------------------

/// Register + setup + confirm TOTP; returns (token, base32 secret, backup codes).
async fn enroll_totp(env: &Env, username: &str, email: &str) -> (String, String, Vec<String>) {
    let (status, body) = register_user(&env.app, username, email).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/2fa/totp/setup", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let secret = body["secret"].as_str().unwrap().to_string();
    assert!(body["otpauth_uri"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/"));
    assert!(body["qr_data_uri"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));

    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/totp/confirm",
                json!({ "code": current_totp(&secret) }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let codes: Vec<String> = body["backup_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(codes.len(), 10);
    for code in &codes {
        assert_eq!(code.len(), 14, "{code}"); // xxxx-xxxx-xxxx
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() == 4));
    }
    (token, secret, codes)
}

#[tokio::test]
async fn totp_lifecycle_and_login_gating() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, secret, codes) = enroll_totp(&env, "laura", "l@x.com").await;

    // Password login is now gated: 400 + doorkeeper shape + extras.
    let (status, body) = password_login(&env.app, "l@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "mfa_required");
    assert_eq!(
        body["error_description"],
        "Two-factor authentication is required."
    );
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let methods: Vec<&str> = body["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(methods.contains(&"totp"), "{body}");
    assert!(methods.contains(&"backup_code"), "{body}");

    // Wrong code → invalid_grant, and the pending login survives for
    // retries inside the attempt budget.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", "123654")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // A valid code completes the login — normal token pair + session row.
    let code = current_totp(&secret);
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &code)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let access = body["access_token"].as_str().unwrap().to_string();
    assert!(body["refresh_token"].is_string());
    let (status, account) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/accounts/verify_credentials", json!({})),
            &access,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    let sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sessions s JOIN users u ON u.id = s.user_id WHERE u.email = 'l@x.com'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(sessions, 1);

    // The consumed mfa_token is dead.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &code)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");

    // Replay of the same TOTP code (same timestep as the success above)
    // is rejected — the matched timestep is not newer than
    // `last_verified_at`. This exercises the inline `code` path too.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("username", "l@x.com"),
                ("password", "hunter2password"),
                ("code", &code),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // Inline one-shot: password + backup code in a single grant — the
    // `code` param is tried as TOTP first, then as a backup code.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/token",
            &[
                ("grant_type", "password"),
                ("username", "l@x.com"),
                ("password", "hunter2password"),
                ("code", &codes[0]),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["access_token"].is_string());
}

#[tokio::test]
async fn pending_login_attempt_budget() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, secret, _codes) = enroll_totp(&env, "mia", "mia@x.com").await;

    let (status, body) = password_login(&env.app, "mia@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();

    // Five failed attempts exhaust the budget — the pending login is
    // dropped, so even a correct code then fails.
    for _ in 0..5 {
        let (status, body) = request(
            &env.app,
            form_req(
                "/oauth/2fa/verify",
                &[("mfa_token", &mfa_token), ("code", "999999")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_grant");
    }
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &current_totp(&secret))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // The password still works — a fresh pending login is staged.
    let (status, body) = password_login(&env.app, "mia@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "mfa_required");
    assert_ne!(body["mfa_token"].as_str().unwrap(), mfa_token);
}

#[tokio::test]
async fn backup_code_lifecycle() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, _secret, codes) = enroll_totp(&env, "nora", "n@x.com").await;

    // Codes are hashed at rest — the plaintext never touches the table.
    let hashes: Vec<String> = sqlx::query_scalar("SELECT code_hash FROM backup_codes ORDER BY id")
        .fetch_all(&env.state.db)
        .await
        .unwrap();
    assert_eq!(hashes.len(), 10);
    for (hash, code) in hashes.iter().zip(&codes) {
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, code);
    }

    // A backup code passes login where TOTP would (via the exchange
    // endpoint), and is consumed — the second use fails.
    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &codes[0])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &codes[0])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Concurrent submissions of the same code race for the atomic
    // UPDATE — exactly one may win.
    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let t1 = body["mfa_token"].as_str().unwrap().to_string();
    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let t2 = body["mfa_token"].as_str().unwrap().to_string();
    let (r1, r2) = tokio::join!(
        request(
            &env.app,
            form_req(
                "/oauth/2fa/verify",
                &[("mfa_token", &t1), ("code", &codes[1])]
            ),
        ),
        request(
            &env.app,
            form_req(
                "/oauth/2fa/verify",
                &[("mfa_token", &t2), ("code", &codes[1])]
            ),
        ),
    );
    let oks = [&r1, &r2]
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .count();
    assert_eq!(oks, 1, "consumed code must be single-use: {r1:?} {r2:?}");

    // Regeneration (password required) invalidates the old batch.
    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &codes[2])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = body["access_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/backup_codes",
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_codes: Vec<String> = body["backup_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(new_codes.len(), 10);
    assert!(new_codes.iter().all(|c| !codes.contains(c)));

    // An old-batch code now fails; a new-batch code works.
    let (_status, body) = password_login(&env.app, "n@x.com", "hunter2password").await;
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, _) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &codes[3])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &new_codes[0])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn totp_setup_requires_secret_key() {
    let mut config = base_config();
    config.auth.secret_key = None;
    let _permit = harness::acquire().await;
    let Some(env) = test_app(config).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "olga", "o@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap();
    // Server misconfiguration → 500, not a client error.
    let (status, _) = request(
        &env.app,
        authed(json_req("POST", "/api/v1/2fa/totp/setup", json!({})), token),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn secret_is_encrypted_at_rest() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, secret, _codes) = enroll_totp(&env, "pete", "p@x.com").await;
    let stored: String = sqlx::query_scalar(
        "SELECT t.totp_secret FROM two_factor t \
         JOIN users u ON u.id = t.user_id WHERE u.email = 'p@x.com'",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    // Not the base32 plaintext, and not trivially reversible to it.
    assert_ne!(stored, secret);
    assert!(!stored.contains(&secret));

    // The plaintext never appears in later responses.
    let (_status, body) = password_login(&env.app, "p@x.com", "hunter2password").await;
    assert!(!body.to_string().contains(&secret));
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &current_totp(&secret))],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.to_string().contains(&secret));
}

// ---------------------------------------------------------------------------
// WebAuthn
// ---------------------------------------------------------------------------

fn soft_token() -> WebauthnAuthenticator<SoftToken> {
    let (token, _ca) = SoftToken::new(false).unwrap();
    WebauthnAuthenticator::new(token)
}

fn rp_origin(env: &Env) -> Url {
    Url::parse(&yonder::activitypub::federation_base_url(
        env.state.config().as_ref(),
    ))
    .unwrap()
}

/// Register a security key for the bearer `token`; returns the new
/// credential row id.
async fn register_key(
    env: &Env,
    token: &str,
    soft: &mut WebauthnAuthenticator<SoftToken>,
    name: &str,
) -> Value {
    let (status, body) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/2fa/webauthn/register/begin", json!({})),
            token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ccr: CreationChallengeResponse = serde_json::from_value(body).unwrap();
    let reg = soft.do_registration(rp_origin(env), ccr).unwrap();
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/webauthn/register",
                json!({
                    "name": name,
                    "credential": serde_json::to_value(reg).unwrap(),
                }),
            ),
            token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// A completed pending login's `mfa_token` for a webauthn-only user.
async fn mfa_login(env: &Env, email: &str) -> String {
    let (status, body) = password_login(&env.app, email, "hunter2password").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "mfa_required");
    body["mfa_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn webauthn_register_assert_login_remove() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "quinn", "q@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap().to_string();
    let mut soft = soft_token();

    let body = register_key(&env, &token, &mut soft, "my yubikey").await;
    let cred_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["name"], "my yubikey");

    // A second key on the same authenticator + listing.
    let body2 = register_key(&env, &token, &mut soft, "spare").await;
    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/2fa/webauthn/credentials", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let creds = body.as_array().unwrap();
    assert_eq!(creds.len(), 2, "{body}");

    // Login is now gated on webauthn.
    let mfa_token = mfa_login(&env, "q@x.com").await;
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert/begin",
            &[("mfa_token", &mfa_token)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rcr: RequestChallengeResponse = serde_json::from_value(body).unwrap();
    let assertion = soft.do_authentication(rp_origin(&env), rcr).unwrap();
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert",
            &[
                ("mfa_token", &mfa_token),
                ("credential", &serde_json::to_string(&assertion).unwrap()),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());

    // last_used_at is bumped on the used key.
    let used: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webauthn_credentials WHERE last_used_at IS NOT NULL",
    )
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(used, 1);

    // Garbage assertion → invalid_grant, counted as a failed attempt.
    let mfa_token = mfa_login(&env, "q@x.com").await;
    request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert/begin",
            &[("mfa_token", &mfa_token)],
        ),
    )
    .await;
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert",
            &[("mfa_token", &mfa_token), ("credential", "{\"id\":\"x\"}")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // Rename works.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                &format!("/api/v1/2fa/webauthn/credentials/{cred_id}/rename"),
                json!({ "name": "primary" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Removing a key requires the password; 403 without it.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/2fa/webauthn/credentials/{cred_id}"),
                json!({ "password": "wrong-password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/2fa/webauthn/credentials/{cred_id}"),
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A different pending login can still use the remaining key.
    let mfa_token = mfa_login(&env, "q@x.com").await;
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert/begin",
            &[("mfa_token", &mfa_token)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rcr: RequestChallengeResponse = serde_json::from_value(body).unwrap();
    let assertion = soft.do_authentication(rp_origin(&env), rcr).unwrap();
    let (status, _) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert",
            &[
                ("mfa_token", &mfa_token),
                ("credential", &serde_json::to_string(&assertion).unwrap()),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Removing the last key disables 2FA entirely.
    let spare_id = body2["id"].as_str().unwrap().to_string();
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/2fa/webauthn/credentials/{spare_id}"),
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = password_login(&env.app, "q@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["access_token"].is_string());
}

#[tokio::test]
async fn webauthn_last_method_clears_backup_codes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    // TOTP first (generates backup codes), then a key; removing both
    // must clear the codes.
    let (token, _secret, codes) = enroll_totp(&env, "rose", "r@x.com").await;
    assert_eq!(codes.len(), 10);
    let mut soft = soft_token();
    let body = register_key(&env, &token, &mut soft, "key").await;
    let cred_id = body["id"].as_str().unwrap().to_string();

    // Remove TOTP — key remains, codes survive.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                "/api/v1/2fa/totp",
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_codes")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, 10);

    // Remove the last method — the batch is dropped.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                &format!("/api/v1/2fa/webauthn/credentials/{cred_id}"),
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_codes")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn webauthn_duplicate_credential_conflict() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_status, body) = register_user(&env.app, "sara", "s@x.com").await;
    let token = body["access_token"].as_str().unwrap().to_string();
    let mut soft = soft_token();
    register_key(&env, &token, &mut soft, "key").await;

    // The credential_id UNIQUE constraint guards duplicates — a raw
    // duplicate INSERT fails specifically as a unique violation.
    let dup = sqlx::query(
        "INSERT INTO webauthn_credentials (user_id, credential_id, public_key, name) \
         SELECT user_id, credential_id, public_key, 'dup' FROM webauthn_credentials LIMIT 1",
    )
    .execute(&env.state.db)
    .await;
    assert!(
        matches!(dup.unwrap_err(), sqlx::Error::Database(ref db) if db.is_unique_violation()),
        "credential_id must be unique"
    );

    // The endpoint's 409 mapping lives in `insert_credential` — a
    // duplicate cred-id surfaces as `Error::Conflict`. (SoftToken mints
    // a fresh random cred-id per ceremony, so a real duplicate through
    // the HTTP ceremony can only come from this path.)
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email = 's@x.com'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    let (cred_id, blob): (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT credential_id, public_key FROM webauthn_credentials WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    let key: webauthn_rs::prelude::SecurityKey = serde_json::from_slice(&blob).unwrap();
    let err =
        yonder::auth::webauthn::insert_credential(&env.state.db, user_id, &cred_id, &key, "dup")
            .await
            .unwrap_err();
    assert!(matches!(err, yonder::Error::Conflict(_)), "{err}");
}

// ---------------------------------------------------------------------------
// Disable semantics / no-2FA / admin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn user_without_2fa_is_unaffected() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, _) = register_user(&env.app, "tina", "t@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = password_login(&env.app, "t@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["access_token"].is_string());
    let (status, body) = request(
        &env.app,
        authed(
            json_req("GET", "/api/v1/2fa", json!({})),
            body["access_token"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["methods"], json!([]));
    assert_eq!(body["totp"]["enabled"], false);
    assert_eq!(body["backup_codes_remaining"], 0);
}

#[tokio::test]
async fn webauthn_register_validates_name() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "zoe", "z@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap().to_string();
    let mut soft = soft_token();

    let (status, body) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/2fa/webauthn/register/begin", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ccr: CreationChallengeResponse = serde_json::from_value(body).unwrap();
    let reg = soft.do_registration(rp_origin(&env), ccr).unwrap();

    // An over-long name is a 422 — and the staged ceremony survives,
    // so a corrected retry still completes.
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/webauthn/register",
                json!({
                    "name": "x".repeat(600),
                    "credential": serde_json::to_value(&reg).unwrap(),
                }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/webauthn/register",
                json!({
                    "name": "ok",
                    "credential": serde_json::to_value(&reg).unwrap(),
                }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An empty rename is a 422 too.
    let cred_id = body["id"].as_str().unwrap();
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                &format!("/api/v1/2fa/webauthn/credentials/{cred_id}/rename"),
                json!({ "name": "  " }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn webauthn_assert_begin_edge_cases() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, _secret, _codes) = enroll_totp(&env, "amy", "a@x.com").await;

    // A TOTP-only user has no credentials to assert → invalid_grant.
    let mfa_token = mfa_login(&env, "a@x.com").await;
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert/begin",
            &[("mfa_token", &mfa_token)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_grant");

    // An unknown/expired mfa_token is invalid_grant on both endpoints.
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/webauthn/assert/begin",
            &[("mfa_token", "deadbeef")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", "deadbeef"), ("code", "123456")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn totp_setup_conflict_and_delete_404() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (status, body) = register_user(&env.app, "yuri", "y@x.com").await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap().to_string();

    // No TOTP enrolled — DELETE is a 404 even with the right password.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                "/api/v1/2fa/totp",
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Confirming without a staged setup is a 404.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/totp/confirm",
                json!({ "code": "000000" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An enrolled user gets a 409 on a second setup.
    let (token, _secret, _codes) = enroll_totp(&env, "yana", "y2@x.com").await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/2fa/totp/setup", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn disable_all_semantics() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (token, _secret, codes) = enroll_totp(&env, "uma", "u@x.com").await;

    // Password required — 403 without it.
    let (status, _) = request(
        &env.app,
        authed(
            json_req("DELETE", "/api/v1/2fa", json!({ "password": "nope" })),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // keep_backup_codes retains the batch.
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                "/api/v1/2fa",
                json!({ "password": "hunter2password", "keep_backup_codes": true }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_codes")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, 10);
    let totp_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM two_factor")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(totp_rows, 0);

    // Login is ungated again.
    let (status, body) = password_login(&env.app, "u@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Re-enrollment replaces the retained batch: an old code is dead,
    // a code from the fresh batch works.
    let (status, body) = request(
        &env.app,
        authed(
            json_req("POST", "/api/v1/2fa/totp/setup", json!({})),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_secret = body["secret"].as_str().unwrap().to_string();
    let (status, body) = request(
        &env.app,
        authed(
            json_req(
                "POST",
                "/api/v1/2fa/totp/confirm",
                json!({ "code": current_totp(&new_secret) }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_codes: Vec<String> = body["backup_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    let (_status, body) = password_login(&env.app, "u@x.com", "hunter2password").await;
    let mfa_token = body["mfa_token"].as_str().unwrap().to_string();
    let (status, _) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &codes[0])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = request(
        &env.app,
        form_req(
            "/oauth/2fa/verify",
            &[("mfa_token", &mfa_token), ("code", &new_codes[0])],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn disable_all_drops_backup_codes() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (token, _secret, _codes) = enroll_totp(&env, "vera", "v@x.com").await;
    let (status, _) = request(
        &env.app,
        authed(
            json_req(
                "DELETE",
                "/api/v1/2fa",
                json!({ "password": "hunter2password" }),
            ),
            &token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM backup_codes")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn admin_disable_2fa_primitive() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (_token, _secret, _codes) = enroll_totp(&env, "wade", "w@x.com").await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE email = 'w@x.com'")
        .fetch_one(&env.state.db)
        .await
        .unwrap();
    yonder::auth::two_factor::admin_disable_2fa(&env.state.db, user_id)
        .await
        .unwrap();
    for table in ["two_factor", "webauthn_credentials", "backup_codes"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&env.state.db)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} not cleared");
    }
    let (status, body) = password_login(&env.app, "w@x.com", "hunter2password").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn totp_status_endpoint() {
    let _permit = harness::acquire().await;
    let Some(env) = test_app(base_config()).await else {
        return;
    };
    let (token, _secret, codes) = enroll_totp(&env, "xena", "x@x.com").await;
    let (status, body) = request(
        &env.app,
        authed(json_req("GET", "/api/v1/2fa", json!({})), &token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["totp"]["enabled"], true);
    let methods: Vec<&str> = body["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(methods.contains(&"totp"));
    assert!(methods.contains(&"backup_code"));
    assert_eq!(body["backup_codes_remaining"], codes.len() as i64);
}
