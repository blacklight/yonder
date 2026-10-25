import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { Notification } from "./entities";

export type NotificationsQuery = {
  max_id?: string;
  min_id?: string;
  since_id?: string;
  limit?: number;
  /** Repeated `types[]` filter (mention, follow, favourite, …). */
  types?: string[];
  /** Repeated `exclude_types[]` filter. */
  exclude_types?: string[];
};

/** `GET /api/v1/notifications` — paginated; auth required. */
export async function notifications(
  query?: NotificationsQuery,
): Promise<Page<Notification>> {
  const { data, response } = await requestWithResponse<Notification[]>({
    path: "/notifications",
    query: {
      max_id: query?.max_id,
      min_id: query?.min_id,
      since_id: query?.since_id,
      limit: query?.limit,
      "types[]": query?.types,
      "exclude_types[]": query?.exclude_types,
    },
  });
  return paginated(response, data);
}

/**
 * `GET /api/v1/notifications/unread_count` — Yonder extension;
 * unwraps `{unread_count}`.
 */
export async function unreadCount(): Promise<number> {
  const body = await request<{ unread_count: number }>({
    path: "/notifications/unread_count",
  });
  return body.unread_count;
}

/** `POST /api/v1/notifications/clear` — dismiss everything. */
export function clearNotifications(): Promise<void> {
  return request<void>({ path: "/notifications/clear", method: "POST" });
}

/** `POST /api/v1/notifications/{id}/dismiss` — dismiss one. */
export function dismissNotification(id: string): Promise<void> {
  return request<void>({
    path: `/notifications/${encodeURIComponent(id)}/dismiss`,
    method: "POST",
  });
}
