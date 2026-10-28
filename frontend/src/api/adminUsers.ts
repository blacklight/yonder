import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { AdminRole } from "./admin";

/**
 * `Admin::Account` — the moderation-state view returned by
 * `GET /api/v1/admin/accounts` and every admin account action.
 * `email`/`email_verified`/`approved` are null on remote accounts
 * (no local `users` row).
 */
export interface AdminAccount {
  id: string;
  username: string;
  domain: string | null;
  role: "user" | "moderator" | "admin" | string;
  email?: string | null;
  email_verified?: boolean | null;
  approved?: boolean | null;
  suspended: boolean;
  limited: boolean;
  disabled: boolean;
  force_sensitive: boolean;
  created_at: string;
}

const path = (id: string | number) =>
  `/admin/accounts/${encodeURIComponent(String(id))}`;

/** `GET /api/v1/admin/accounts` filter params (Mastodon's vocabulary). */
export interface AdminAccountFilters {
  origin?: "local" | "remote";
  status?: "active" | "pending" | "disabled" | "silenced" | "suspended";
  role?: AdminRole;
  staff?: boolean;
  /** Substring match on username, display name and email. */
  search?: string;
  by_domain?: string;
  limit?: number;
  max_id?: string;
}

function filterQuery(f: AdminAccountFilters) {
  return {
    origin: f.origin,
    status: f.status,
    role: f.role,
    staff: f.staff || undefined,
    // Yonder's `search` param ORs the term across username, display
    // name and email (the panel's single box); Mastodon's separate
    // username/display_name/email params remain available verbatim.
    search: f.search,
    by_domain: f.by_domain,
    limit: f.limit,
    max_id: f.max_id,
  };
}

/** One page of the filtered account index (`max_id` pages older). */
export async function listAdminAccounts(
  filters: AdminAccountFilters = {},
): Promise<Page<AdminAccount>> {
  const res = await requestWithResponse<AdminAccount[]>({
    path: "/admin/accounts",
    query: filterQuery(filters),
  });
  return paginated(res.response, res.data);
}

export function showAdminAccount(id: string | number): Promise<AdminAccount> {
  return request({ path: path(id) });
}

export interface AdminCreateAccountBody {
  username: string;
  email: string;
  password: string;
  role?: AdminRole;
  /** Create the account already verified (implied for staff roles). */
  skip_verification?: boolean;
  locale?: string;
}

/**
 * `POST /api/v1/admin/accounts` — Yonder extension: create a local
 * account bypassing the registration mode. Admin-only.
 */
export function createAdminAccount(
  body: AdminCreateAccountBody,
): Promise<AdminAccount> {
  return request({ path: "/admin/accounts", method: "POST", body });
}

/**
 * `DELETE /api/v1/admin/accounts/{id}` — REST alias for the
 * `type: "delete"` account action. Admin-only.
 */
export function deleteAdminAccount(
  id: string | number,
): Promise<{ id: string; deleted: boolean }> {
  return request({ path: path(id), method: "DELETE" });
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
