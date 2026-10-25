import { request } from "./client";

/**
 * `Admin::Account` — the moderation-state view returned by
 * `GET /api/v1/admin/accounts/{id}` and every admin account action.
 */
export interface AdminAccount {
  id: string;
  username: string;
  domain: string | null;
  role: "user" | "moderator" | "admin" | string;
  suspended: boolean;
  limited: boolean;
  disabled: boolean;
  force_sensitive: boolean;
  created_at: string;
}

const path = (id: string | number) =>
  `/admin/accounts/${encodeURIComponent(String(id))}`;

export function showAdminAccount(id: string | number): Promise<AdminAccount> {
  return request({ path: path(id) });
}

/** Mastodon reversal verbs: unsilence | unsuspend | unsensitive | enable. */
export type AdminReversal =
  "unsilence" | "unsuspend" | "unsensitive" | "enable";

export function reverseAdminAction(
  id: string | number,
  verb: AdminReversal,
): Promise<AdminAccount> {
  return request({ path: `${path(id)}/${verb}`, method: "POST", body: {} });
}

/**
 * `POST …/reset_password` — returns a one-time reset link (also mailed
 * when `email.send_password_reset` is on). Moderator+; self-targets,
 * suspended/deleted and remote accounts are rejected server-side.
 */
export function resetPasswordLink(
  id: string | number,
): Promise<{ reset_url: string }> {
  return request({
    path: `${path(id)}/reset_password`,
    method: "POST",
    body: {},
  });
}

/** `POST …/email_reset` — always mails the link (`sent: true`). */
export function emailResetLink(
  id: string | number,
): Promise<{ sent: boolean }> {
  return request({
    path: `${path(id)}/email_reset`,
    method: "POST",
    body: {},
  });
}
