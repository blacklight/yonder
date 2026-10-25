import { defineStore } from "pinia";
import { ref } from "vue";
import { clearNotifications, unreadCount } from "@/api/notifications";
import { useAuthStore } from "./auth";
import { useStreamingStore } from "./streaming";

const REFETCH_DEBOUNCE_MS = 1000;

/**
 * The unread-notification badge. `start()` (called once per app
 * lifetime from the layout) fetches the count and subscribes to the
 * `user` stream: each `notification` event re-fetches the count,
 * debounced so a burst doesn't storm the endpoint.
 */
export const useNotificationsStore = defineStore("notifications", () => {
  const unread = ref(0);
  let off: (() => void) | null = null;
  let debounce: ReturnType<typeof setTimeout> | null = null;

  async function refresh() {
    if (!useAuthStore().isLoggedIn) {
      unread.value = 0;
      return;
    }
    try {
      unread.value = await unreadCount();
    } catch {
      // Keep the last count on transient errors.
    }
  }

  /** Idempotent: begins the badge feed for a logged-in session. */
  function start() {
    if (off || !useAuthStore().isLoggedIn) return;
    void refresh();
    off = useStreamingStore().subscribe("user", (ev) => {
      if (ev.event !== "notification") return;
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => {
        debounce = null;
        void refresh();
      }, REFETCH_DEBOUNCE_MS);
    });
  }

  function stop() {
    off?.();
    off = null;
    if (debounce) {
      clearTimeout(debounce);
      debounce = null;
    }
    unread.value = 0;
  }

  /** `POST /notifications/clear` + zero the badge. */
  async function clear() {
    await clearNotifications();
    unread.value = 0;
  }

  return { unread, refresh, start, stop, clear };
});
