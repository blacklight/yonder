import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { Account, ListEntity } from "./entities";

export type ListBody = {
  title: string;
  replies_policy?: "followed" | "list" | "none";
  exclusive?: boolean;
};

/** `GET /api/v1/lists` — the caller's lists. */
export function listLists(): Promise<ListEntity[]> {
  return request<ListEntity[]>({ path: "/lists" });
}

/** `POST /api/v1/lists`. */
export function createList(body: ListBody): Promise<ListEntity> {
  return request<ListEntity>({ path: "/lists", method: "POST", body });
}

/** `GET /api/v1/lists/{id}`. */
export function getList(id: string): Promise<ListEntity> {
  return request<ListEntity>({ path: `/lists/${encodeURIComponent(id)}` });
}

/** `PUT /api/v1/lists/{id}`. */
export function updateList(id: string, body: ListBody): Promise<ListEntity> {
  return request<ListEntity>({
    path: `/lists/${encodeURIComponent(id)}`,
    method: "PUT",
    body,
  });
}

/** `DELETE /api/v1/lists/{id}`. */
export function deleteList(id: string): Promise<unknown> {
  return request<unknown>({
    path: `/lists/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

/** `GET /api/v1/lists/{id}/accounts` — paginated members. */
export async function listMembers(
  id: string,
  query?: {
    max_id?: string;
    min_id?: string;
    since_id?: string;
    limit?: number;
  },
): Promise<Page<Account>> {
  const { data, response } = await requestWithResponse<Account[]>({
    path: `/lists/${encodeURIComponent(id)}/accounts`,
    query,
  });
  return paginated(response, data);
}

/** `POST /api/v1/lists/{id}/accounts` — JSON `{account_ids}`. */
export function addListMembers(
  id: string,
  accountIds: string[],
): Promise<unknown> {
  return request<unknown>({
    path: `/lists/${encodeURIComponent(id)}/accounts`,
    method: "POST",
    body: { account_ids: accountIds },
  });
}

/** `DELETE /api/v1/lists/{id}/accounts` — JSON `{account_ids}`. */
export function removeListMembers(
  id: string,
  accountIds: string[],
): Promise<unknown> {
  return request<unknown>({
    path: `/lists/${encodeURIComponent(id)}/accounts`,
    method: "DELETE",
    body: { account_ids: accountIds },
  });
}
