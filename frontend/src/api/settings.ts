import { request, requestBlob } from "./client";
import type {
  AuthorizedApp,
  CreatedToken,
  Filter,
  FilterKeyword,
  HistoricalObservation,
  HistoryMessage,
  ImportConnection,
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
/** `POST /api/v1/yonder/imports` — multipart: kind, mode, federate, file. */
export function createImport(
  kind: string,
  mode: "merge" | "overwrite",
  file: File,
  federate = false,
): Promise<TransferRow> {
  const form = new FormData();
  form.set("kind", kind);
  form.set("mode", mode);
  if (federate) form.set("federate", "true");
  form.set("file", file);
  return request<TransferRow>({
    path: "/yonder/imports",
    method: "POST",
    body: form,
  });
}

/** `POST /api/v1/yonder/imports/{id}/cancel`. */
export function cancelImport(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/yonder/imports/${encodeURIComponent(id)}/cancel`,
    method: "POST",
  });
}
/** `POST /api/v1/yonder/imports/{id}/resume` — optional connection rebind. */
export function resumeImport(
  id: string,
  connectionId?: string,
): Promise<unknown> {
  return request<unknown>({
    path: `/yonder/imports/${encodeURIComponent(id)}/resume`,
    method: "POST",
    // Snowflake ids exceed Number.MAX_SAFE_INTEGER — send the decimal
    // string; the backend accepts string or number.
    body: connectionId ? { connection_id: connectionId } : {},
  });
}

// Remote OAuth/API import (epic 31 §9)
export function listImportConnections(): Promise<ImportConnection[]> {
  return request<ImportConnection[]>({ path: "/yonder/import_connections" });
}
export function beginImportConnection(
  origin: string,
  categories: string[],
): Promise<{ authorization_url: string }> {
  return request<{ authorization_url: string }>({
    path: "/yonder/import_connections",
    method: "POST",
    body: { origin, categories },
  });
}
export function revokeImportConnection(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/yonder/import_connections/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}
export function createImportSession(opts: {
  /** Snowflake id as decimal string — never `Number()`. */
  connection_id: string;
  categories: string[];
  limit?: number;
  order?: "newest" | "oldest";
  since?: string;
  until?: string;
  federate?: boolean;
  /** Replace destination profile values even when the source sends empty ones. */
  overwrite_profile?: boolean;
}): Promise<{ id: string; state: string }> {
  return request<{ id: string; state: string }>({
    path: "/yonder/import_sessions",
    method: "POST",
    body: opts,
  });
}

// Historical overlay reads (epic 31 §10) — owner-scoped.
export function statusHistory(id: string): Promise<HistoricalObservation[]> {
  return request<HistoricalObservation[]>({
    path: `/yonder/statuses/${encodeURIComponent(id)}/history`,
  });
}
export function historyMessages(maxId?: string): Promise<HistoryMessage[]> {
  return request<HistoryMessage[]>({
    path: "/yonder/history/messages",
    query: maxId ? { max_id: maxId } : {},
  });
}
export function historyMedia(id: string, index: number) {
  return requestBlob({
    path: `/yonder/history/messages/${encodeURIComponent(id)}/media/${index}`,
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

export async function listAliases(): Promise<string[]> {
  const body = await request<{ aliases: string[] } | string[]>({
    path: "/yonder/aliases",
  });
  return Array.isArray(body) ? body : (body.aliases ?? []);
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
