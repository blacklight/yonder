import { requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";

/** Audit-log row — admin-only (epic 22). */
export interface AuditEntry {
  id: string;
  actor_account_id: string | null;
  actor_account: {
    id: string;
    username: string;
    domain: string | null;
  } | null;
  action: string;
  target_type: string;
  target_id: string | null;
  details: Record<string, unknown>;
  created_at: string;
}

export interface AuditFilters {
  /** Literal prefix, e.g. `report.` or `config.`. */
  action?: string;
  actor_account_id?: string;
  target_type?: string;
  target_id?: string;
  /** RFC 3339 bounds. */
  since?: string;
  until?: string;
  max_id?: string;
  min_id?: string;
  limit?: number;
}

/** `GET /api/v1/admin/audit_log` — keyset-paginated, newest first. */
export async function listAuditLog(
  filters: AuditFilters = {},
): Promise<Page<AuditEntry>> {
  const { data, response } = await requestWithResponse<AuditEntry[]>({
    path: "/admin/audit_log",
    query: { ...filters },
  });
  return paginated(response, data);
}
