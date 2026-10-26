import { request } from "./client";
import type {
  AuthorizedApp,
  CreatedToken,
  Filter,
  FilterKeyword,
  Invite,
  Preferences,
  SessionEntity,
  Token,
  TransferRow,
  TwoFactorStatus,
  WebauthnCredential,
  YonderSettings,
} from "./entities";

/** `GET /api/v1/preferences` — Mastodon Preferences entity. */
export function preferences(): Promise<Preferences> {
  return request<Preferences>({ path: "/preferences" });
}

/** `GET /api/v1/yonder/settings` — the full Yonder settings object. */
export function getYonderSettings(): Promise<YonderSettings> {
  return request<YonderSettings>({ path: "/yonder/settings" });
}

/**
 * `PATCH /api/v1/yonder/settings` — send ONLY the changed keys
 * (`null` resets a key to the instance default).
 */
export function patchYonderSettings(
  patch: Record<string, unknown>,
): Promise<YonderSettings> {
  return request<YonderSettings>({
    path: "/yonder/settings",
    method: "PATCH",
    body: patch,
  });
}

// ---------------------------------------------------------------------------
// Filters (/api/v2/filters)
// ---------------------------------------------------------------------------

export function listFilters(): Promise<Filter[]> {
  return request<Filter[]>({ path: "/api/v2/filters" });
}
export function createFilter(body: {
  title: string;
  context: string[];
  filter_action?: string;
  expires_in?: number;
}): Promise<Filter> {
  return request<Filter>({ path: "/api/v2/filters", method: "POST", body });
}
export function updateFilter(
  id: string,
  body: {
    title?: string;
    context?: string[];
    filter_action?: string;
    expires_in?: number;
  },
): Promise<Filter> {
  return request<Filter>({
    path: `/api/v2/filters/${encodeURIComponent(id)}`,
    method: "PUT",
    body,
  });
}
export function deleteFilter(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/api/v2/filters/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}
export function addFilterKeyword(
  filterId: string,
  keyword: string,
  wholeWord = true,
): Promise<FilterKeyword> {
  return request<FilterKeyword>({
    path: `/api/v2/filters/${encodeURIComponent(filterId)}/keywords`,
    method: "POST",
    body: { keyword, whole_word: wholeWord },
  });
}
export function deleteFilterKeyword(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/api/v2/filters/keywords/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

// ---------------------------------------------------------------------------
// Security: sessions, tokens, 2FA, OAuth apps
// ---------------------------------------------------------------------------

export function listSessions(): Promise<SessionEntity[]> {
  return request<SessionEntity[]>({ path: "/sessions" });
}
export function revokeSession(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/sessions/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

export function listTokens(): Promise<Token[]> {
  return request<Token[]>({ path: "/tokens" });
}
export function createToken(body: {
  name: string;
  scopes: string;
  expires_in?: number;
}): Promise<CreatedToken> {
  return request<CreatedToken>({ path: "/tokens", method: "POST", body });
}
export function revokeToken(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/tokens/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

export function twoFactorStatus(): Promise<TwoFactorStatus> {
  return request<TwoFactorStatus>({ path: "/2fa" });
}
export function totpSetup(): Promise<{
  secret: string;
  otpauth_uri: string;
  qr_data_uri: string;
}> {
  return request({ path: "/2fa/totp/setup", method: "POST" });
}
export function totpConfirm(code: string): Promise<{ backup_codes: string[] }> {
  return request({ path: "/2fa/totp/confirm", method: "POST", body: { code } });
}
/** Password-gated. */
export function totpDelete(password: string): Promise<unknown> {
  return request({
    path: "/2fa/totp",
    method: "DELETE",
    body: { password },
  });
}
/** Password-gated; codes shown once from the response. */
export function regenerateBackupCodes(
  password: string,
): Promise<{ backup_codes: string[] }> {
  return request({
    path: "/2fa/backup_codes",
    method: "POST",
    body: { password },
  });
}
export function webauthnRegisterBegin(): Promise<unknown> {
  return request({ path: "/2fa/webauthn/register/begin", method: "POST" });
}
export function webauthnRegister(
  credential: unknown,
  name?: string,
): Promise<unknown> {
  return request({
    path: "/2fa/webauthn/register",
    method: "POST",
    body: { credential, name },
  });
}
export function webauthnCredentials(): Promise<WebauthnCredential[]> {
  return request<WebauthnCredential[]>({ path: "/2fa/webauthn/credentials" });
}
export function webauthnRename(id: string, name: string): Promise<unknown> {
  return request({
    path: `/2fa/webauthn/credentials/${encodeURIComponent(id)}/rename`,
    method: "POST",
    body: { name },
  });
}
/** Password-gated. */
export function webauthnDelete(id: string, password: string): Promise<unknown> {
  return request({
    path: `/2fa/webauthn/credentials/${encodeURIComponent(id)}`,
    method: "DELETE",
    body: { password },
  });
}
/** Password-gated; disables every second factor. */
export function disableTwoFactor(
  password: string,
  keepBackupCodes?: boolean,
): Promise<unknown> {
  return request({
    path: "/2fa",
    method: "DELETE",
    body: { password, keep_backup_codes: keepBackupCodes },
  });
}

export function listAuthorizedApps(): Promise<AuthorizedApp[]> {
  return request<AuthorizedApp[]>({ path: "/oauth/authorized_applications" });
}
export function revokeAuthorizedApp(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/oauth/authorized_applications/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

// ---------------------------------------------------------------------------
// Data: exports / imports (download blobs via `requestBlob`)
// ---------------------------------------------------------------------------

export function listExports(): Promise<TransferRow[]> {
  return request<TransferRow[]>({ path: "/yonder/exports" });
}
export function getExport(id: string): Promise<TransferRow> {
  return request<TransferRow>({
    path: `/yonder/exports/${encodeURIComponent(id)}`,
  });
}
export function listImports(): Promise<TransferRow[]> {
  return request<TransferRow[]>({ path: "/yonder/imports" });
}
/** `POST /api/v1/yonder/imports` — multipart: kind, mode, file. */
export function createImport(
  kind: string,
  mode: "merge" | "overwrite",
  file: File,
): Promise<TransferRow> {
  const form = new FormData();
  form.set("kind", kind);
  form.set("mode", mode);
  form.set("file", file);
  return request<TransferRow>({
    path: "/yonder/imports",
    method: "POST",
    body: form,
  });
}

// ---------------------------------------------------------------------------
// Account: invites, aliases, move, delete
// ---------------------------------------------------------------------------

export function listInvites(): Promise<Invite[]> {
  return request<Invite[]>({ path: "/yonder/invites" });
}
export function createInvite(body: {
  max_uses?: number | null;
  expires_at?: string | null;
  expires_in?: number;
}): Promise<Invite> {
  return request<Invite>({ path: "/yonder/invites", method: "POST", body });
}
export function revokeInvite(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/yonder/invites/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

export function listAliases(): Promise<string[]> {
  return request<string[]>({ path: "/yonder/aliases" });
}
export function addAlias(acct: string): Promise<unknown> {
  return request<unknown>({
    path: "/yonder/aliases",
    method: "POST",
    body: { acct },
  });
}
export function removeAlias(acct: string): Promise<unknown> {
  return request<unknown>({
    path: "/yonder/aliases",
    method: "DELETE",
    body: { acct },
  });
}

/** `POST /api/v1/yonder/move` — password-gated account redirect. */
export function moveAccount(acct: string, password: string): Promise<unknown> {
  return request<unknown>({
    path: "/yonder/move",
    method: "POST",
    body: { acct, password },
  });
}
/** `DELETE /api/v1/yonder/move` — password-gated cancel. */
export function cancelMove(password: string): Promise<unknown> {
  return request<unknown>({
    path: "/yonder/move",
    method: "DELETE",
    body: { password },
  });
}

/**
 * `POST /api/v1/yonder/account/password` — change the account
 * password. Revokes every session/token but the caller's.
 */
export function changePassword(
  currentPassword: string,
  newPassword: string,
): Promise<{ ok: boolean }> {
  return request<{ ok: boolean }>({
    path: "/yonder/account/password",
    method: "POST",
    body: { current_password: currentPassword, new_password: newPassword },
  });
}

/** `POST /api/v1/yonder/account/delete` — schedules deletion after the grace period. */
export function deleteAccount(password: string): Promise<unknown> {
  return request<unknown>({
    path: "/yonder/account/delete",
    method: "POST",
    body: { password },
  });
}
