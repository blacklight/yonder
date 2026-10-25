import { defineStore } from "pinia";
import { ref } from "vue";
import {
  conversationStatuses,
  deleteConversation,
  listConversations,
  markConversationRead,
} from "@/api/conversations";
import type { Conversation, Status } from "@/api/entities";
import { useAuthStore } from "./auth";
import { resetOnUserChange } from "./perUser";
import { useStreamingStore } from "./streaming";

const REFETCH_DEBOUNCE_MS = 1000;

/**
 * DM conversation list + open thread. Live updates come from the
 * `user` stream only: direct statuses arrive as `update` events (the
 * `direct` stream is accepted by the socket but never published, and
 * DMs never produce `notification` events). The client can't derive
 * conversation keys, so a direct `update` debounce-refetches the first
 * list page (and the open thread's first page). `delete` events carry
 * only a status id and fire for every home-timeline deletion — a
 * refetch happens only when the id is known to this store.
 */
export const useConversationsStore = defineStore("conversations", () => {
  const items = ref<Conversation[]>([]);
  const nextCursor = ref<string | undefined>();
  const loading = ref(false);
  const error = ref<string | null>(null);
  const activeThread = ref<{ id: string; statuses: Status[]; next?: string } | null>(null);
  let off: (() => void) | null = null;
  let debounce: ReturnType<typeof setTimeout> | null = null;

  function reset() {
    items.value = [];
    nextCursor.value = undefined;
    error.value = null;
    activeThread.value = null;
  }
  resetOnUserChange(reset);

  function cursorParam(url?: string) {
    if (!url) return undefined;
    return (
      new URL(url, location.origin).searchParams.get("max_id") ?? undefined
    );
  }

  async function fetchPage(fresh = false) {
    if (!useAuthStore().isLoggedIn) return;
    loading.value = true;
    error.value = null;
    try {
      const page = await listConversations({
        max_id: fresh ? undefined : cursorParam(nextCursor.value),
      });
      items.value = fresh ? page.items : [...items.value, ...page.items];
      nextCursor.value = page.next;
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e);
    } finally {
      loading.value = false;
    }
  }

  async function refresh() {
    if (!useAuthStore().isLoggedIn) return;
    try {
      // Merge the fresh first page into the loaded list rather than
      // replacing it — wholesale replacement would drop pages the
      // user already paged in.
      const page = await listConversations({});
      const fresh = new Set(page.items.map((c) => c.id));
      items.value = [
        ...page.items,
        ...items.value.filter((c) => !fresh.has(c.id)),
      ];
    } catch {
      /* keep the current list on transient errors */
    }
    const t = activeThread.value;
    if (t) {
      try {
        const page = await conversationStatuses(t.id);
        t.statuses = page.items;
        t.next = page.next;
      } catch {
        /* leave stale thread on transient errors */
      }
    }
  }

  async function openThread(id: string) {
    const page = await conversationStatuses(id);
    activeThread.value = { id, statuses: page.items, next: page.next };
    await markConversationRead(id).catch(() => {});
    const conv = items.value.find((c) => c.id === id);
    if (conv) conv.unread = false;
  }

  async function fetchOlder() {
    const t = activeThread.value;
    if (!t?.next) return;
    const page = await conversationStatuses(t.id, { max_id: cursorParam(t.next) });
    t.statuses = [...t.statuses, ...page.items];
    t.next = page.next;
  }

  function closeThread() {
    activeThread.value = null;
  }

  /** `DELETE /conversations/{id}` — caller-only hide. */
  async function remove(id: string) {
    await deleteConversation(id);
    items.value = items.value.filter((c) => c.id !== id);
    if (activeThread.value?.id === id) activeThread.value = null;
  }

  function knownStatusId(id: unknown): boolean {
    if (typeof id !== "string" && typeof id !== "number") return false;
    const sid = String(id);
    if (items.value.some((c) => c.last_status?.id === sid)) return true;
    return activeThread.value?.statuses.some((s) => s.id === sid) ?? false;
  }

  function onEvent(ev: { event: string; payload: unknown }) {
    const direct =
      ev.event === "update" &&
      (ev.payload as { visibility?: string })?.visibility === "direct";
    const knownDelete = ev.event === "delete" && knownStatusId(ev.payload);
    if (!direct && !knownDelete) return;
    if (debounce) clearTimeout(debounce);
    debounce = setTimeout(() => {
      debounce = null;
      void refresh();
    }, REFETCH_DEBOUNCE_MS);
  }

  function start() {
    if (off || !useAuthStore().isLoggedIn) return;
    void fetchPage(true);
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

  return {
    items,
    nextCursor,
    loading,
    error,
    activeThread,
    fetchPage,
    refresh,
    openThread,
    fetchOlder,
    closeThread,
    remove,
    start,
    stop,
    reset,
  };
});
