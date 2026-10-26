import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { Conversation, Status } from "./entities";

export type ConversationsQuery = {
  max_id?: string;
  min_id?: string;
  since_id?: string;
  limit?: number;
};

/** `GET /api/v1/conversations` — the caller's DM conversations. */
export async function listConversations(
  query?: ConversationsQuery,
): Promise<Page<Conversation>> {
  const { data, response } = await requestWithResponse<Conversation[]>({
    path: "/conversations",
    query,
  });
  return paginated(response, data);
}

/** `GET /api/v1/conversations/{id}/statuses` — the thread, paginated. */
export async function conversationStatuses(
  id: string,
  query?: ConversationsQuery,
): Promise<Page<Status>> {
  const { data, response } = await requestWithResponse<Status[]>({
    path: `/conversations/${encodeURIComponent(id)}/statuses`,
    query,
  });
  return paginated(response, data);
}

/** `POST /api/v1/conversations/{id}/read` — mark the thread read. */
export function markConversationRead(id: string): Promise<Conversation> {
  // Best-effort read marker — the caller swallows failures already.
  return request<Conversation>({
    path: `/conversations/${encodeURIComponent(id)}/read`,
    method: "POST",
    silent: true,
  });
}

/** `DELETE /api/v1/conversations/{id}` — caller-only hide. */
export function deleteConversation(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/conversations/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}
