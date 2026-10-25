import { request, requestWithResponse, type RequestOptions } from "./client";
import { paginated, type Page } from "./pagination";
import type {
  EmojiReaction,
  MediaAttachment,
  Poll,
  ScheduledStatus,
  Status,
  StatusCreateBody,
  StatusVisibility,
} from "./entities";

/** Cursor/limit query shared by every timeline endpoint. */
export type TimelineQuery = {
  max_id?: string;
  min_id?: string;
  since_id?: string;
  limit?: number;
};

async function pageOfStatuses(
  path: string,
  query?: RequestOptions["query"],
): Promise<Page<Status>> {
  const { data, response } = await requestWithResponse<Status[]>({
    path,
    query,
  });
  return paginated(response, data);
}

/** `GET /api/v1/timelines/home` — the caller's home feed. */
export function homeTimeline(query?: TimelineQuery): Promise<Page<Status>> {
  return pageOfStatuses("/timelines/home", query);
}

export type PublicTimelineQuery = TimelineQuery & {
  local?: boolean;
  remote?: boolean;
  only_media?: boolean;
};

/** `GET /api/v1/timelines/public` — local or federated feed. */
export function publicTimeline(
  query?: PublicTimelineQuery,
): Promise<Page<Status>> {
  return pageOfStatuses("/timelines/public", query);
}

/** `GET /api/v1/timelines/tag/{tag}` — hashtag feed. */
export function tagTimeline(
  tag: string,
  query?: TimelineQuery & { local?: boolean },
): Promise<Page<Status>> {
  return pageOfStatuses(`/timelines/tag/${encodeURIComponent(tag)}`, query);
}

/** `GET /api/v1/timelines/list/{id}` — a list's feed (auth). */
export function listTimeline(
  id: string,
  query?: TimelineQuery,
): Promise<Page<Status>> {
  return pageOfStatuses(`/timelines/list/${encodeURIComponent(id)}`, query);
}

/** `GET /api/v1/statuses/{id}`. */
export function getStatus(id: string): Promise<Status> {
  return request<Status>({ path: `/statuses/${encodeURIComponent(id)}` });
}

/**
 * `POST /api/v1/statuses`. `idempotencyKey` rides the
 * `Idempotency-Key` header — the backend replays the stored result
 * for a repeated key, so retries never double-post. A body with
 * `scheduled_at` comes back as a `ScheduledStatus`, not a `Status`.
 */
export function createStatus(
  body: StatusCreateBody,
  idempotencyKey?: string,
): Promise<Status | ScheduledStatus> {
  return request<Status | ScheduledStatus>({
    path: "/statuses",
    method: "POST",
    body,
    headers: idempotencyKey ? { "Idempotency-Key": idempotencyKey } : undefined,
  });
}

/** `PUT /api/v1/statuses/{id}` — edit own status. */
export function editStatus(
  id: string,
  body: Partial<StatusCreateBody>,
): Promise<Status> {
  return request<Status>({
    path: `/statuses/${encodeURIComponent(id)}`,
    method: "PUT",
    body,
  });
}

/** `DELETE /api/v1/statuses/{id}` — owner delete (tombstone). */
export function deleteStatus(id: string): Promise<Status> {
  return request<Status>({
    path: `/statuses/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}

function postStatusAction<T = Status>(id: string, action: string): Promise<T> {
  return request<T>({
    path: `/statuses/${encodeURIComponent(id)}/${action}`,
    method: "POST",
  });
}

export function favourite(id: string): Promise<Status> {
  return postStatusAction(id, "favourite");
}
export function unfavourite(id: string): Promise<Status> {
  return postStatusAction(id, "unfavourite");
}
export function reblog(
  id: string,
  visibility?: StatusVisibility,
): Promise<Status> {
  return request<Status>({
    path: `/statuses/${encodeURIComponent(id)}/reblog`,
    method: "POST",
    body: visibility ? { visibility } : undefined,
  });
}
export function unreblog(id: string): Promise<Status> {
  return postStatusAction(id, "unreblog");
}
export function bookmark(id: string): Promise<Status> {
  return postStatusAction(id, "bookmark");
}
export function unbookmark(id: string): Promise<Status> {
  return postStatusAction(id, "unbookmark");
}
export function pin(id: string): Promise<Status> {
  return postStatusAction(id, "pin");
}
export function unpin(id: string): Promise<Status> {
  return postStatusAction(id, "unpin");
}

function reactionPath(id: string, emoji?: string): string {
  const base = `/pleroma/statuses/${encodeURIComponent(id)}/reactions`;
  return emoji === undefined ? base : `${base}/${encodeURIComponent(emoji)}`;
}

/** `PUT /api/v1/pleroma/statuses/{id}/reactions/{emoji}` — react. */
export function react(id: string, emoji: string): Promise<Status> {
  return request<Status>({ path: reactionPath(id, emoji), method: "PUT" });
}

/** `DELETE .../reactions/{emoji}` — remove the caller's reaction. */
export function unreact(id: string, emoji: string): Promise<Status> {
  return request<Status>({ path: reactionPath(id, emoji), method: "DELETE" });
}

/** `GET .../reactions` — the reaction summary array for a status. */
export function reactionsList(id: string): Promise<EmojiReaction[]> {
  return request<EmojiReaction[]>({ path: reactionPath(id) });
}

/** `POST /api/v1/polls/{id}/votes` — body `{choices: [idx…]}`; returns the updated Poll. */
export function votePoll(id: string, choices: number[]): Promise<Poll> {
  return request<Poll>({
    path: `/polls/${encodeURIComponent(id)}/votes`,
    method: "POST",
    body: { choices },
  });
}

/**
 * `POST /api/v2/media` — multipart upload; returns the entity
 * synchronously (v2 delegates to the v1 pipeline server-side).
 */
export function uploadMedia(
  file: File | Blob,
  opts: { description?: string; focus?: string; filename?: string } = {},
): Promise<MediaAttachment> {
  const form = new FormData();
  form.set(
    "file",
    file,
    opts.filename ?? (file instanceof File ? file.name : "upload"),
  );
  if (opts.description) form.set("description", opts.description);
  if (opts.focus) form.set("focus", opts.focus);
  return request<MediaAttachment>({
    path: "/api/v2/media",
    method: "POST",
    body: form,
  });
}

/** `GET /api/v2/media/{id}` — processing status (owner only). */
export function mediaProcessing(id: string): Promise<MediaAttachment> {
  return request<MediaAttachment>({
    path: `/api/v2/media/${encodeURIComponent(id)}`,
  });
}

/** `PUT /api/v1/media/{id}` — update alt text / focus. */
export function updateMedia(
  id: string,
  body: { description?: string; focus?: string },
): Promise<MediaAttachment> {
  return request<MediaAttachment>({
    path: `/media/${encodeURIComponent(id)}`,
    method: "PUT",
    body,
  });
}

/** `GET /api/v1/scheduled_statuses`. */
export function scheduledStatuses(): Promise<ScheduledStatus[]> {
  return request<ScheduledStatus[]>({ path: "/scheduled_statuses" });
}

/** `DELETE /api/v1/scheduled_statuses/{id}` — cancel. */
export function deleteScheduledStatus(id: string): Promise<void> {
  return request<void>({
    path: `/scheduled_statuses/${encodeURIComponent(id)}`,
    method: "DELETE",
  });
}
