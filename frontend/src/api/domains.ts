import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";

/** `GET /api/v1/domain_blocks` — the caller's blocked domains. */
export function domainBlocks(): Promise<string[]> {
  return request<string[]>({ path: "/domain_blocks" });
}

/** `GET /api/v1/domain_blocks` — paginated (Link headers). */
export async function domainBlocksPage(query?: {
  max_id?: string;
  min_id?: string;
  since_id?: string;
  limit?: number;
}): Promise<Page<string>> {
  const { data, response } = await requestWithResponse<string[]>({
    path: "/domain_blocks",
    query,
  });
  return paginated(response, data);
}

/** `POST /api/v1/domain_blocks` — `domain` in the JSON body. */
export function blockDomain(domain: string): Promise<void> {
  return request<void>({
    path: "/domain_blocks",
    method: "POST",
    body: { domain },
  });
}

/**
 * `DELETE /api/v1/domain_blocks` — the backend reads `domain` from
 * the body or query on the collection route (no `/{domain}` path).
 */
export function unblockDomain(domain: string): Promise<void> {
  return request<void>({
    path: "/domain_blocks",
    method: "DELETE",
    body: { domain },
  });
}
