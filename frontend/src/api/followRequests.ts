import { request } from "./client";
import type { Account } from "./entities";

/** `POST /api/v1/follow_requests/{id}/authorize`. */
export function authorizeFollowRequest(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/follow_requests/${encodeURIComponent(id)}/authorize`,
    method: "POST",
  });
}

/** `POST /api/v1/follow_requests/{id}/reject`. */
export function rejectFollowRequest(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/follow_requests/${encodeURIComponent(id)}/reject`,
    method: "POST",
  });
}

/** `GET /api/v1/follow_requests` — pending requests as accounts. */
export function listFollowRequests(): Promise<Account[]> {
  return request<Account[]>({ path: "/follow_requests" });
}
