import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type {
  Account,
  CredentialAccount,
  Relationship,
  Status,
} from "./entities";

/** `GET /api/v1/accounts/verify_credentials` — the caller's account. */
export function verifyCredentials(): Promise<CredentialAccount> {
  return request<CredentialAccount>({
    path: "/accounts/verify_credentials",
  });
}

/** `GET /api/v1/accounts/{id}`. */
export function getAccount(id: string): Promise<Account> {
  return request<Account>({ path: `/accounts/${encodeURIComponent(id)}` });
}

/** `GET /api/v1/accounts/relationships?id[]=…` — batch relationships. */
export function getRelationships(ids: string[]): Promise<Relationship[]> {
  return request<Relationship[]>({
    path: "/accounts/relationships",
    query: { "id[]": ids },
  });
}

function accountAction<T>(id: string, action: string): Promise<T> {
  return request<T>({
    path: `/accounts/${encodeURIComponent(id)}/${action}`,
    method: "POST",
  });
}

export function follow(id: string): Promise<Relationship> {
  return accountAction(id, "follow");
}
export function unfollow(id: string): Promise<Relationship> {
  return accountAction(id, "unfollow");
}
export function muteAccount(id: string): Promise<Relationship> {
  return accountAction(id, "mute");
}
export function unmuteAccount(id: string): Promise<Relationship> {
  return accountAction(id, "unmute");
}
export function blockAccount(id: string): Promise<Relationship> {
  return accountAction(id, "block");
}
export function unblockAccount(id: string): Promise<Relationship> {
  return accountAction(id, "unblock");
}

/**
 * `POST /api/v1/accounts/{id}/note` — the caller's private note on the
 * account; returns the Relationship entity (which carries `note`).
 */
export function setNote(id: string, comment: string): Promise<Relationship> {
  return request<Relationship>({
    path: `/accounts/${encodeURIComponent(id)}/note`,
    method: "POST",
    body: { comment },
  });
}

export type AccountStatusesQuery = {
  max_id?: string;
  min_id?: string;
  since_id?: string;
  limit?: number;
  only_media?: boolean;
  exclude_replies?: boolean;
  exclude_reblogs?: boolean;
  pinned?: boolean;
};

/** `GET /api/v1/accounts/{id}/statuses` — paginated. */
export async function accountStatuses(
  id: string,
  query?: AccountStatusesQuery,
): Promise<Page<Status>> {
  const { data, response } = await requestWithResponse<Status[]>({
    path: `/accounts/${encodeURIComponent(id)}/statuses`,
    query,
  });
  return paginated(response, data);
}
