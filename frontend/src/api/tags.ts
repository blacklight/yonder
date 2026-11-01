import { request, requestWithResponse } from "./client";
import { paginated, type Page } from "./pagination";
import type { Tag } from "./entities";

/** `GET /api/v1/trends/tags` query — the page's filter controls map
 *  straight onto these Yonder window extensions. */
export type TrendingTagsQuery = {
  /** `day`/`week`/`month` window length (server default `week`). */
  period?: "day" | "week" | "month";
  /** `latest` ends the window at the most recent hashtag occurrence
   *  rather than now — the page's presets use it so a quiet instance
   *  still ranks its last active tags. */
  anchor?: "now" | "latest";
  /** Restrict the ranking to local authors. */
  local?: boolean;
  /** Explicit window bounds (RFC 3339 or `YYYY-MM-DD`); either one
   *  overrides `period`/`anchor`. */
  from?: string;
  to?: string;
  limit?: number;
  offset?: number;
};

/** `GET /api/v1/trends/tags` — ranked by uses, descending. */
export function trendingTags(query?: TrendingTagsQuery): Promise<Tag[]> {
  return request<Tag[]>({ path: "/trends/tags", query });
}

/** `GET /api/v1/followed_tags` — the caller's followed tags,
 *  cursored on `Link` headers. */
export async function followedTags(query?: {
  max_id?: string;
  limit?: number;
}): Promise<Page<Tag>> {
  const { data, response } = await requestWithResponse<Tag[]>({
    path: "/followed_tags",
    query,
  });
  return paginated(response, data);
}

/** `GET /api/v1/tags/{name}` — `following` is viewer-specific. */
export function getTag(name: string): Promise<Tag> {
  return request<Tag>({ path: `/tags/${encodeURIComponent(name)}` });
}

/** `POST /api/v1/tags/{name}/follow` — idempotent. */
export function followTag(name: string): Promise<Tag> {
  return request<Tag>({
    path: `/tags/${encodeURIComponent(name)}/follow`,
    method: "POST",
  });
}

/** `POST /api/v1/tags/{name}/unfollow` — idempotent. */
export function unfollowTag(name: string): Promise<Tag> {
  return request<Tag>({
    path: `/tags/${encodeURIComponent(name)}/unfollow`,
    method: "POST",
  });
}
