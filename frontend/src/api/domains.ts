import { request } from "./client";

/** `GET /api/v1/domain_blocks` — the caller's blocked domains. */
export function domainBlocks(): Promise<string[]> {
  return request<string[]>({ path: "/domain_blocks" });
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
