import { request } from "./client";

export interface ReportBody {
  /** Numeric account id (serialized as a number — the backend is i64). */
  account_id: number;
  status_ids?: number[];
  comment?: string;
  category?: string;
  forward?: boolean;
}

/** `POST /api/v1/reports` — file a report; returns the Report entity. */
export function fileReport(body: ReportBody): Promise<unknown> {
  return request({ path: "/reports", method: "POST", body });
}
