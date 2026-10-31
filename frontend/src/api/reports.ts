import { request } from "./client";

export interface ReportBody {
  /**
   * Account id as rendered by the entity (`"1234"`). Ids stay strings
   * end-to-end: snowflake values exceed `Number.MAX_SAFE_INTEGER`, so
   * `Number()` would silently corrupt them before the request leaves
   * the browser.
   */
  account_id: string;
  status_ids?: string[];
  comment?: string;
  category?: string;
  forward?: boolean;
}

/** `POST /api/v1/reports` — file a report; returns the Report entity. */
export function fileReport(body: ReportBody): Promise<unknown> {
  return request({ path: "/reports", method: "POST", body });
}
