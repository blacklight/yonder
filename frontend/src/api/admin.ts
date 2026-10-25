import { request } from "./client";
import type { AdminAccount } from "./adminUsers";

export type AdminStatusAction = "sensitive" | "delete";

/**
 * `POST /api/v1/admin/statuses/{id}/action` — Yonder extension
 * (epic 24): mark a status sensitive or tombstone it. Moderator+.
 */
export function adminStatusAction(
  id: string,
  type: AdminStatusAction,
): Promise<{ id: string; action: string; applied: boolean }> {
  return request({
    path: `/admin/statuses/${encodeURIComponent(id)}/action`,
    method: "POST",
    body: { type },
  });
}

export interface AdminAccountActionBody {
  /** `none|sensitive|disable|silence|suspend|limit|delete`. */
  type: string;
  report_id?: number;
  text?: string;
  send_email_notification?: boolean;
}

/**
 * `POST /api/v1/admin/accounts/{id}/action` — the account-level
 * moderation surface (returns the Admin::Account entity).
 */
export function adminAccountAction(
  id: string | number,
  body: AdminAccountActionBody,
): Promise<AdminAccount> {
  return request<AdminAccount>({
    path: `/admin/accounts/${encodeURIComponent(String(id))}/action`,
    method: "POST",
    body,
  });
}

export type AdminRole = "user" | "moderator" | "admin";

/**
 * `POST /api/v1/admin/accounts/{id}/role` — promote/demote.
 * Admin scope; 409 when demoting the last active admin, 422 on an
 * invalid role.
 */
export function setRole(
  id: string | number,
  role: AdminRole,
): Promise<AdminAccount> {
  return request<AdminAccount>({
    path: `/admin/accounts/${encodeURIComponent(String(id))}/role`,
    method: "POST",
    body: { role },
  });
}
