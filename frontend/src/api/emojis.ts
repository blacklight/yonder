import { request } from "./client";
import type { CustomEmoji } from "./entities";

// The instance's emoji set is stable for a session — every picker
// open would otherwise re-fetch the same list.
let cached: Promise<CustomEmoji[]> | null = null;

/** `GET /api/v1/custom_emojis` — anonymous; the instance's emoji set. */
export function customEmojis(): Promise<CustomEmoji[]> {
  cached ??= request<CustomEmoji[]>({
    path: "/custom_emojis",
    skipAuth: true,
  });
  return cached;
}
