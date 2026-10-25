import { defineStore } from "pinia";
import { ref } from "vue";
import {
  clearNotifications,
  dismissNotification,
  notifications as fetchNotifications,
  unreadCount,
} from "@/api/notifications";
import type { Notification } from "@/api/entities";
import { useAuthStore } from "./auth";
import { resetOnUserChange } from "./perUser";
import { useStreamingStore } from "./streaming";

const REFETCH_DEBOUNCE_MS = 1000;

/** Filter aliases → backend `types[]` values (`v1::notifications`). */
export const FILTERS = {
  all: [] as string[],
  likes: ["favourite"],
  boosts: ["reblog"],
  quotes: ["quote"],
  mentions: ["mention"],
  reports: ["admin.report"],
} as const;
export type NotificationFilter = keyof typeof FILTERS;

/**
 * The unread-notification badge plus the page list. `start()` (called
 * once per app lifetime from the layout) fetches the count and
 * subscribes to the `user` stream: `notification` events refresh the
 * badge (debounced) and live-insert the row when it passes the
 * active filter. There is no mark-read endpoint — `clear` deletes.
 */
export const useNotificationsStore = defineStore("notifications", () => {
  const unread = ref(0);
  const items = ref<Notification[]>([]);
  const activeFilter = ref<NotificationFilter>("all");
  const nextCursor = ref<string | undefined>();
  const loading = ref(false);
  const error = ref<string | null>(null);
  let off: (() => void) | null = null;
  let debounce: ReturnType<typeof setTimeout> | null = null;

  function reset() {
    items.value = [];
    activeFilter.value = "all";
    nextCursor.value = undefined;
    error.value = null;
    unread.value = 0;
  }
  resetOnUserChange(reset);

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

  /** Fetch a page; `reset: true` restarts the list (filter change/open). */
  async function fetchPage(fresh = false) {
    const auth = useAuthStore();
    if (!auth.isLoggedIn) return;
    if (fresh) {
      items.value = [];
      nextCursor.value = undefined;
    }
    loading.value = true;
    error.value = null;
    try {
      const types = FILTERS[activeFilter.value];
      const page = await fetchNotifications({
        types: types.length ? [...types] : undefined,
        max_id: nextCursor.value
          ? (new URL(nextCursor.value, location.origin).searchParams.get(
              "max_id",
            ) ?? undefined)
          : undefined,
      });
      items.value = fresh ? page.items : [...items.value, ...page.items];
      nextCursor.value = page.next;
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e);
    } finally {
      loading.value = false;
    }
  }

  function setFilter(f: NotificationFilter) {
    if (f === activeFilter.value) return;
    activeFilter.value = f;
    void fetchPage(true);
  }

  /** A streamed event passes the filter when its type is included. */
  function passesFilter(n: Notification): boolean {
    const types = FILTERS[activeFilter.value];
    return types.length === 0 || (types as readonly string[]).includes(n.type);
  }

  function onEvent(ev: { event: string; payload: unknown }) {
    if (ev.event !== "notification") return;
    const n = ev.payload as Notification;
    if (n && typeof n === "object" && n.id && passesFilter(n)) {
      if (!items.value.some((i) => i.id === n.id)) items.value.unshift(n);
    }
    if (debounce) clearTimeout(debounce);
    debounce = setTimeout(() => {
      debounce = null;
      void refresh();
    }, REFETCH_DEBOUNCE_MS);
  }

  /** Idempotent: begins the badge feed for a logged-in session. */
  function start() {
    if (off || !useAuthStore().isLoggedIn) return;
    void refresh();
    off = useStreamingStore().subscribe("user", onEvent);
  }

  function stop() {
    off?.();
    off = null;
    if (debounce) {
      clearTimeout(debounce);
      debounce = null;
    }
    reset();
  }

  /** `POST /notifications/clear` — deletes rows (never "mark read"). */
  async function clear() {
    await clearNotifications();
    items.value = [];
    unread.value = 0;
  }

  /** `POST /notifications/{id}/dismiss` — remove one row. */
  async function dismiss(id: string) {
    await dismissNotification(id);
    items.value = items.value.filter((n) => n.id !== id);
    void refresh();
  }

  return {
    unread,
    items,
    activeFilter,
    nextCursor,
    loading,
    error,
    refresh,
    fetchPage,
    setFilter,
    start,
    stop,
    clear,
    dismiss,
  };
});
