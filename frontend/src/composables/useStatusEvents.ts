import { onBeforeUnmount, onMounted } from "vue";
import type { Status } from "@/api/entities";
import { useAuthStore } from "@/stores/auth";
import { useStreamingStore, type StreamEvent } from "@/stores/streaming";

export interface StatusEventHandlers {
  /**
   * `update`/`status.update` entity — patch matching rows in place
   * (`patchStatus`/`patchStatusList`). Also fires for new posts the
   * page does not display, so handlers must never insert.
   */
  onStatus?: (status: Status) => void;
  /** `delete` — the bare status id (`dropFromStatusList`). */
  onDelete?: (id: string) => void;
}

/**
 * Live status events for pages that display statuses outside the
 * timeline store (profile, bookmarks, search results, quotes, …).
 *
 * Subscribes to `public` plus the viewer's `user` stream — the two
 * streams every status event reaches: public posts land on `public`;
 * followers-only and direct posts only ever reach the author's and
 * their followers' `user` streams, which includes the viewer whenever
 * the post is visible to them at all. The `TimelinePage` and
 * `StatusPage` keep their own subscriptions (the former routes
 * through the timeline store's pending buffer, the latter appends
 * fresh replies).
 */
export function useStatusEvents(handlers: StatusEventHandlers): void {
  const streaming = useStreamingStore();
  const auth = useAuthStore();
  const offs: (() => void)[] = [];

  function onEvent(ev: StreamEvent) {
    if (ev.event === "delete") {
      handlers.onDelete?.(String(ev.payload));
      return;
    }
    if (
      (ev.event === "update" || ev.event === "status.update") &&
      ev.payload !== null &&
      typeof ev.payload === "object"
    ) {
      handlers.onStatus?.(ev.payload as Status);
    }
  }

  onMounted(() => {
    offs.push(streaming.subscribe("public", onEvent));
    if (auth.isLoggedIn) offs.push(streaming.subscribe("user", onEvent));
  });
  onBeforeUnmount(() => offs.splice(0).forEach((f) => f()));
}
