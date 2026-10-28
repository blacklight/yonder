import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { TokenBody } from "./oauth";
import type {
  Account,
  CredentialAccount,
  ListEntity,
  Relationship,
  Status,
} from "./entities";

/** `POST /api/v1/accounts` — Mastodon registration shape. */
export interface RegisterBody {
  username: string;
  email: string;
  password: string;
  /** Required by the backend — the form binds it to a rules checkbox. */
  agreement: boolean;
  locale?: string;
  /** Registration reason (approval-required instances). */
  reason?: string;
  /** Bypasses closed registration; auto-approves on approval-required. */
  invite_code?: string;
}

/**
 * Register a new account. Anonymous on purpose (`skipAuth`): an
 * optional bearer *app* token may be sent, but a user token would be
 * rejected as a non-app credential. Returns the first token entity —
 * sign in with `auth.signInWithToken(res.access_token)`. The backend
 * adds `verification_pending` when email confirmation gates posting.
 */
export function register(
  body: RegisterBody,
): Promise<TokenBody & { verification_pending?: boolean }> {
  return request<TokenBody>({
    path: "/accounts",
    method: "POST",
    body,
    skipAuth: true,
  });
}

/**
 * `POST /api/v1/accounts/verify_email` — consume the emailed token.
 * Anonymous (`skipAuth`): the `/verify-email/:token` link is opened
 * before the user has signed in.
 */
export function verifyEmail(token: string): Promise<{ verified: boolean }> {
  return request<{ verified: boolean }>({
    path: "/accounts/verify_email",
    method: "POST",
    body: { token },
    skipAuth: true,
  });
}

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

/** `GET /api/v1/accounts/lookup?acct=` — handle → Account (epic 25). */
export function lookup(acct: string): Promise<Account> {
  return request<Account>({
    path: "/accounts/lookup",
    query: { acct },
    skipAuth: false,
  });
}

/** `GET /api/v1/accounts/relationships?id[]=…` — batch relationships. */
export function getRelationships(ids: string[]): Promise<Relationship[]> {
  return request<Relationship[]>({
    path: "/accounts/relationships",
    query: { "id[]": ids },
  });
}

function accountAction<T>(
  id: string,
  action: string,
  body?: unknown,
): Promise<T> {
  return request<T>({
    path: `/accounts/${encodeURIComponent(id)}/${action}`,
    method: "POST",
    body,
  });
}

/**
 * `POST /api/v1/accounts/{id}/follow` — optional `{reblogs, notify}`
 * flags (`body_params` on the backend). Omitted keys keep the
 * backend defaults.
 */
export function follow(
  id: string,
  opts?: { reblogs?: boolean; notify?: boolean },
): Promise<Relationship> {
  const body: Record<string, boolean> = {};
  if (opts?.reblogs !== undefined) body.reblogs = opts.reblogs;
  if (opts?.notify !== undefined) body.notify = opts.notify;
  return accountAction(id, "follow", body);
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

/** `POST /api/v1/accounts/{id}/subscribe` — notify on new posts. */
export function subscribe(id: string): Promise<Relationship> {
  return accountAction(id, "subscribe");
}
/** `POST /api/v1/accounts/{id}/unsubscribe`. */
export function unsubscribe(id: string): Promise<Relationship> {
  return accountAction(id, "unsubscribe");
}

/** `GET /api/v1/accounts/{id}/followers` — paginated accounts. */
export async function followers(
  id: string,
  query?: {
    max_id?: string;
    min_id?: string;
    since_id?: string;
    limit?: number;
  },
): Promise<Page<Account>> {
  const { data, response } = await requestWithResponse<Account[]>({
    path: `/accounts/${encodeURIComponent(id)}/followers`,
    query,
  });
  return paginated(response, data);
}
/** `GET /api/v1/accounts/{id}/following` — paginated accounts. */
export async function following(
  id: string,
  query?: {
    max_id?: string;
    min_id?: string;
    since_id?: string;
    limit?: number;
  },
): Promise<Page<Account>> {
  const { data, response } = await requestWithResponse<Account[]>({
    path: `/accounts/${encodeURIComponent(id)}/following`,
    query,
  });
  return paginated(response, data);
}

/** `GET /api/v1/accounts/{id}/lists` — lists this account is a member of. */
export function getAccountLists(id: string): Promise<ListEntity[]> {
  return request<ListEntity[]>({
    path: `/accounts/${encodeURIComponent(id)}/lists`,
  });
}

/**
 * `PATCH /api/v1/accounts/update_credentials` — JSON normally;
 * `FormData` only when avatar/header files are attached (the only
 * transport that accepts files).
 */
export function updateCredentials(
  body: Record<string, unknown> | FormData,
): Promise<CredentialAccount> {
  return request<CredentialAccount>({
    path: "/accounts/update_credentials",
    method: "PATCH",
    body,
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
