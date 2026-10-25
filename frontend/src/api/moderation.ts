import { request } from "./client";
import type { Account } from "./entities";

/** `Admin::Report` — moderator+ (epic 21). */
export interface AdminReport {
  id: string;
  action_taken: boolean;
  action_taken_at: string | null;
  category: string;
  comment: string;
  forwarded: boolean;
  created_at: string;
  account_id: string;
  account: Account | null;
  target_account: Account | null;
  status_id: string | null;
  assigned_account: Account | null;
}

export interface ReportListParams {
  resolved?: boolean;
  account_id?: string;
  target_account_id?: string;
  max_id?: string;
  since_id?: string;
  limit?: number;
}

const path = (id: string | number) =>
  `/admin/reports/${encodeURIComponent(String(id))}`;

/** `GET /api/v1/admin/reports` — open reports by default. */
export function listAdminReports(
  params: ReportListParams = {},
): Promise<AdminReport[]> {
  return request({ path: "/admin/reports", query: { ...params } });
}

export function getAdminReport(id: string | number): Promise<AdminReport> {
  return request({ path: path(id) });
}

/** Mark handled; 409 when already resolved. */
export function resolveReport(id: string | number): Promise<AdminReport> {
  return request({ path: `${path(id)}/resolve`, method: "POST", body: {} });
}

export function reopenReport(id: string | number): Promise<AdminReport> {
  return request({ path: `${path(id)}/reopen`, method: "POST", body: {} });
}

export function assignReportToSelf(id: string | number): Promise<AdminReport> {
  return request({
    path: `${path(id)}/assign_to_self`,
    method: "POST",
    body: {},
  });
}

export function unassignReport(id: string | number): Promise<AdminReport> {
  return request({ path: `${path(id)}/unassign`, method: "POST", body: {} });
}
