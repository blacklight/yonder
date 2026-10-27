# Authentication

Yonder implements Mastodon-compatible authentication: account
registration, OAuth2 token issuance, scoped/revocable bearer tokens,
session tracking, and optional two-factor authentication.

## Secrets at rest

| Material | Storage |
|---|---|
| Passwords | argon2id (`users.password_hash`) |
| Bearer tokens, client secrets, auth codes | `hex(sha256(secret))` — uniformly random material doesn't need a KDF |
| TOTP secrets | ChaCha20-Poly1305, keyed via `HKDF-SHA256(auth.secret_key, salt="yonder:2fa", info="totp-secret")` |
| Backup codes | `hex(sha256(normalized code))` — deterministic equality is required for lookup, and sha256 of a random code is already unrecoverable |

`auth.secret_key` is the HKDF master key for TOTP-secret encryption.
**Set it via `YONDER_AUTH__SECRET_KEY`** (environment), not the TOML
file. TOTP enrollment fails with a 500 and a logged cause while it is
unset — that's a server misconfiguration, not a client error.

## Grants

`POST /oauth/token` supports `password`, `client_credentials`,
`authorization_code` (+PKCE), `refresh_token`, and the RFC 8628 device
flow (`/oauth/device_code` + `/oauth/device/authorize`). Grant errors
use the doorkeeper JSON shape (`{"error", "error_description"}`).

Access tokens never expire by default (`auth.access_token_expiry_minutes
= 0`) — the Mastodon doorkeeper behaviour: sessions stay valid until
revoked, and `expires_in` is omitted from token responses. Setting a
positive value restores hard expiry; refresh tokens
(`auth.refresh_token_expiry_days`, default 30 days) then keep clients
logged in via the `refresh_token` grant.

Interactive grants create a `sessions` row wrapping the access token;
`GET /api/v1/sessions` lists them and `DELETE /api/v1/sessions/{id}`
revokes a login (killing its token).

## Two-factor authentication

Three second factors: **TOTP** (authenticator apps; SHA1/6-digit/30s,
±1 window), **WebAuthn security keys** (YubiKey-class FIDO2,
non-discoverable credentials), and **backup codes** (10 per batch,
`xxxx-xxxx-xxxx`, single-use). "2FA enabled" means a confirmed TOTP
row *or* at least one WebAuthn credential — a staged, unconfirmed TOTP
setup does not count.

### Login flow

With 2FA enabled, the `password` grant no longer issues tokens
directly. Without a `code` form param it answers **400**:

```json
{"error": "mfa_required",
 "error_description": "Two-factor authentication is required.",
 "mfa_token": "<pending token>",
 "methods": ["totp", "webauthn", "backup_code"]}
```

`methods` lists the factors that can complete the login (backup codes
only appear while an unused one remains). The `mfa_token` identifies a
Redis-staged pending login (TTL 300 s) that records the requested
scope, client and an attempt counter — at 5 failed second-factor
attempts it is dropped and the password must be re-authenticated.

Completion paths:

- **Inline one-shot**: pass `code` on the original password grant
  (verified as TOTP first, then as a backup code).
- **`POST /oauth/2fa/verify`** `{mfa_token, code}` — same verification
  order; a backup code is consumed on success.
- **WebAuthn**: `POST /oauth/2fa/webauthn/assert/begin`
  `{mfa_token}` returns assertion options;
  `POST /oauth/2fa/webauthn/assert` `{mfa_token, credential}` (the
  credential field is the JSON-serialized `PublicKeyCredential`)
  verifies the assertion and issues the token pair.

Success consumes the pending login atomically and proceeds exactly
like a normal password grant: token pair + `sessions` row +
`oauth_authorizations` upsert.

Replay protection: a TOTP code verifies only when its matched timestep
is newer than `two_factor.last_verified_at`, updated in the same
conditional `UPDATE` — the same code (or any code in the same window)
can't be replayed, and backup-code consumption is a single atomic
conditional update.

### Management API

All routes are authenticated, inside the `authentication` rate-limit
bucket, and read endpoints require `read` scope while mutations
require `write`. Destructive operations re-authenticate with the
account password in the request body (mismatch → 403).

| Route | Purpose |
|---|---|
| `GET /api/v1/2fa` | `{enabled, methods[], totp: {enabled, last_verified_at}, webauthn: [...], backup_codes_remaining}` |
| `POST /api/v1/2fa/totp/setup` | `{secret (base32, once), otpauth_uri, qr_data_uri}` |
| `POST /api/v1/2fa/totp/confirm` | `{code}` → `{backup_codes}` (shown once) |
| `DELETE /api/v1/2fa/totp` | `{password}` — backup codes die with it when no keys remain |
| `POST /api/v1/2fa/backup_codes` | `{password}` → fresh batch, old hashes deleted |
| `POST /api/v1/2fa/webauthn/register/begin` | creation options (staged ceremony) |
| `POST /api/v1/2fa/webauthn/register` | `{name, credential}` → persist named key |
| `GET /api/v1/2fa/webauthn/credentials` | `{id, name, created_at, last_used_at}` list |
| `POST /api/v1/2fa/webauthn/credentials/{id}/rename` | `{name}` |
| `DELETE /api/v1/2fa/webauthn/credentials/{id}` | `{password}` — last-method rule applies |
| `DELETE /api/v1/2fa` | `{password, keep_backup_codes?}` — disable everything |

Disabling the last method deletes the backup-code batch unless
`keep_backup_codes` is set on `DELETE /api/v1/2fa`. Issued bearer
tokens are unaffected by 2FA changes — 2FA gates *login*, not token
validity (session revocation is the user's lever).

WebAuthn relying-party: `rp_id = instance.domain`, origin =
`federation.base_url` (default `https://{domain}`). rp_id must be an
effective domain of the origin; `http://localhost` works for dev.
`auth::two_factor::admin_disable_2fa(pool, user_id)` is the primitive
the admin panel will call to force-clear a user's factors.

Secrets are never logged and never returned after their one-time
reveal (TOTP secret at setup, backup codes at confirm/regenerate).
