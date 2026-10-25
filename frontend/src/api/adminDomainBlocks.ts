import { request } from "./client";

/**
 * `Admin::DomainBlock` (epic 21). `severity` covers the PROMPT's
 * "suspend / federated-timeline removal / followers-only" spectrum —
 * `silence` hides the domain from public timelines, `suspend` blocks
 * it outright; `reject_media`/`reject_reports` are the other toggles.
 */
export interface AdminDomainBlock {
  id: string;
  domain: string;
  digest: string;
  created_at: string;
  severity: "noop" | "silence" | "suspend" | string;
  reject_media: boolean;
  reject_reports: boolean;
  obfuscate: boolean;
  private_comment: string;
  public_comment: string;
}

export interface DomainBlockBody {
  domain?: string;
  severity?: string;
  reject_media?: boolean;
  reject_reports?: boolean;
  obfuscate?: boolean;
  private_comment?: string;
  public_comment?: string;
}

const path = (id: string | number) =>
  `/admin/domain_blocks/${encodeURIComponent(String(id))}`;

export function listDomainBlocks(
  params: {
    max_id?: string;
    limit?: number;
  } = {},
): Promise<AdminDomainBlock[]> {
  return request({ path: "/admin/domain_blocks", query: { ...params } });
}

/** POST is an upsert on `domain`. */
export function upsertDomainBlock(
  body: DomainBlockBody,
): Promise<AdminDomainBlock> {
  return request({ path: "/admin/domain_blocks", method: "POST", body });
}

export function patchDomainBlock(
  id: string | number,
  body: DomainBlockBody,
): Promise<AdminDomainBlock> {
  return request({ path: path(id), method: "PATCH", body });
}

export function deleteDomainBlock(id: string | number): Promise<object> {
  return request({ path: path(id), method: "DELETE" });
}
