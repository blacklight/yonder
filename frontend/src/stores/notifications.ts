import { defineStore } from "pinia";
import { ref } from "vue";
import {
  clearNotifications,
  dismissNotification,
  markNotificationsRead,
  notifications as fetchNotifications,
  unreadCount,
} from "@/api/notifications";
import type { IconProp } from "@fortawesome/fontawesome-svg-core";
import type { Notification } from "@/api/entities";
import { i18n } from "@/i18n";
import { router } from "@/router";
import { sanitizeHtml } from "@/utils/sanitize";
import { useAuthStore } from "./auth";
import { resetOnUserChange } from "./perUser";
import { useStreamingStore } from "./streaming";
import { useToastsStore } from "./toasts";

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

/** i18n key for a notification's label, `unknown` as the fallback. */
export function notificationTypeKey(n: Notification): string {
  const key = `notifications.type.${n.type.replace(".", "_")}`;
  return i18n.global.te(key) ? key : "notifications.type.unknown";
}

const TYPE_ICONS: Record<string, IconProp> = {
  follow: "user-plus",
  follow_request: "user-plus",
  mention: "at",
  favourite: "star",
  reblog: "retweet",
  quote: "quote-left",
  poll: "square-poll-vertical",
  status: "bell",
  update: "pen",
  emoji_reaction: ["far", "face-smile"],
  "admin.sign_up": "user-plus",
  moderation_warning: "triangle-exclamation",
  "admin.report": "flag",
};

/** The per-type icon shown on rows and toasts. */
export function notificationIcon(n: Notification): IconProp {
  return TYPE_ICONS[n.type] ?? "bell";
}

/** Flat label text for a notification (toast bodies). */
function notificationLabel(n: Notification): string {
  return i18n.global.t(notificationTypeKey(n), {
    name: n.account?.display_name || n.account?.acct || "",
  });
}

/** Where a notification toast should navigate on click. */
function notificationTarget(n: Notification): string {
  if (n.status?.id) return `/statuses/${n.status.id}`;
  if (n.type === "admin.report" && n.report?.id)
    return `/moderation/reports/${n.report.id}`;
  if (n.account?.acct) return `/@${n.account.acct}`;
  return "/notifications";
}

/** Plain-text excerpt of a status body for native notification text. */
function statusExcerpt(n: Notification): string | undefined {
  const html = n.status?.reblog?.content || n.status?.content;
  if (!html) return undefined;
  const doc = new DOMParser().parseFromString(sanitizeHtml(html), "text/html");
  const text = (doc.body.textContent ?? "").replace(/\s+/g, " ").trim();
  return text ? text.slice(0, 140) : undefined;
}

/** The pending one-shot gesture handler asking for permission. */
let permListener: (() => void) | null = null;

/**
 * Browsers only honour `Notification.requestPermission()` inside a
 * user-gesture handler, so the request is armed at `start()` and fires
 * on the next pointer/key interaction. `denied` is sticky and never
 * re-armed.
 */
function armPermissionRequest() {
  if (permListener || !("Notification" in window)) return;
  if (window.Notification.permission !== "default") return;
  const ask = () => {
    disarmPermissionRequest();
    void window.Notification.requestPermission();
  };
  permListener = ask;
  window.addEventListener("pointerdown", ask, { once: true });
  window.addEventListener("keydown", ask, { once: true });
}

function disarmPermissionRequest() {
  if (!permListener) return;
  window.removeEventListener("pointerdown", permListener);
  window.removeEventListener("keydown", permListener);
  permListener = null;
}

/**
 * Native notification for a live event. Only fires while the tab is
 * hidden — a visible page already shows the in-app toast. `tag` keys
 * off the notification id so repeated events replace rather than
 * stack; clicking focuses the tab and navigates to the same target
 * the toast would open.
 */
function browserNotify(n: Notification) {
  if (!("Notification" in window)) return;
  if (window.Notification.permission !== "granted") return;
  if (document.visibilityState === "visible") return;
  const target = notificationTarget(n);
  const native = new window.Notification(notificationLabel(n), {
    body: statusExcerpt(n),
    icon: n.account?.avatar || "/favicon.svg",
    tag: `yonder-notification-${n.id}`,
    data: { url: target },
  });
  native.onclick = () => {
    window.focus();
    void router.push(target);
    native.close();
  };
}

/**
 * The unread-notification badge plus the page list. `start()` (called
 * once per app lifetime from the layout) fetches the count and
 * subscribes to the `user:notification` stream: `notification` events
 * show a toast and live-insert the row when it passes the active
 * filter, `notifications_count` events carry the authoritative badge
 * value. Viewed rows are marked read via `POST /notifications/read`
 * (string ids — snowflakes exceed JS's safe-integer range); `clear`
 * deletes.
 */
export const useNotificationsStore = defineStore("notifications", () => {
  const unread = ref(0);
  const items = ref<Notification[]>([]);
  const activeFilter = ref<NotificationFilter>("all");
  const nextCursor = ref<string | undefined>();
  const loading = ref(false);
  const error = ref<string | null>(null);
  let off: (() => void) | null = null;

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
    // The fanout publishes the authoritative count alongside every
    // notification (and on dismiss/clear) — no refetch needed.
    if (ev.event === "notifications_count") {
      const count = (ev.payload as { unread_count?: unknown })?.unread_count;
      if (typeof count === "number") unread.value = count;
      return;
    }
    if (ev.event !== "notification") return;
    const n = ev.payload as Notification;
    if (!n || typeof n !== "object" || !n.id) return;
    if (passesFilter(n) && !items.value.some((i) => i.id === n.id)) {
      items.value.unshift(n);
    }
    useToastsStore().push({
      text: notificationLabel(n),
      body: statusExcerpt(n),
      to: notificationTarget(n),
      image: n.account?.avatar || undefined,
      icon: notificationIcon(n),
    });
    browserNotify(n);
  }

  /** Idempotent: begins the badge feed for a logged-in session. */
  function start() {
    if (off || !useAuthStore().isLoggedIn) return;
    void refresh();
    armPermissionRequest();
    off = useStreamingStore().subscribe("user:notification", onEvent);
  }

  function stop() {
    off?.();
    off = null;
    disarmPermissionRequest();
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

  /**
   * `POST /notifications/read` — the page posts the ids its viewport
   * rendered. Local rows flip `read` and the badge decrements
   * optimistically; the backend's `notifications_count` push
   * reconciles the exact value.
   */
  async function markRead(ids: string[]) {
    const byId = new Map(items.value.map((n) => [n.id, n]));
    // A missing `read` field (older server) counts as unread.
    const fresh = ids.filter((id) => byId.has(id) && !byId.get(id)?.read);
    if (!fresh.length) return;
    for (const id of fresh) {
      const n = byId.get(id);
      if (n) n.read = true;
    }
    unread.value = Math.max(0, unread.value - fresh.length);
    try {
      await markNotificationsRead(fresh);
    } catch {
      // Read state is cosmetic — a dropped call self-heals on the
      // next count push or page reload.
    }
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
    markRead,
  };
});
