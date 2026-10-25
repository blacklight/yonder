import { request } from "./client";
import type { Account, Status } from "./entities";

/** `GET /api/v2/search` result buckets (v1 returns hashtags as strings). */
export interface SearchResults {
  accounts: Account[];
  statuses: Status[];
  hashtags: { name: string; url?: string; history?: unknown[] }[];
}

export type SearchQuery = {
  q: string;
  type?: "accounts" | "statuses" | "hashtags";
  resolve?: boolean;
  following?: boolean;
  account_id?: string;
  offset?: number;
  limit?: number;
};

/** `GET /api/v2/search` — verbatim `/api/` path (v2 endpoint). */
export function search(query: SearchQuery): Promise<SearchResults> {
  return request<SearchResults>({
    path: "/api/v2/search",
    query: {
      q: query.q,
      type: query.type,
      resolve: query.resolve === true ? "true" : undefined,
      following: query.following === true ? "true" : undefined,
      account_id: query.account_id,
      offset: query.offset,
      limit: query.limit,
    },
  });
}
