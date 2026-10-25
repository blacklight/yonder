import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import type { Conversation } from "@/api/entities";

const api = vi.hoisted(() => ({
  listConversations: vi.fn(),
  conversationStatuses: vi.fn(),
  markConversationRead: vi.fn(),
  deleteConversation: vi.fn(),
}));
vi.mock("@/api/conversations", () => api);

type Handler = (ev: { event: string; payload: unknown }) => void;
const stream = vi.hoisted(() => ({
  handler: null as Handler | null,
  unsubscribed: false,
}));
vi.mock("@/stores/streaming", () => ({
  useStreamingStore: () => ({
    subscribe: vi.fn((_key: string, h: Handler) => {
      stream.handler = h;
      return () => {
        stream.unsubscribed = true;
      };
    }),
  }),
}));

import { useAuthStore } from "./auth";
import { useConversationsStore } from "./conversations";

function conv(id: string, statusId: string): Conversation {
  return {
    id,
    unread: false,
    accounts: [],
    last_status: { id: statusId } as Conversation["last_status"],
  } as Conversation;
}

const page = (items: Conversation[]) => ({ items, next: undefined });

describe("conversations store", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    stream.handler = null;
    stream.unsubscribed = false;
    useAuthStore().setToken("tok");
    api.listConversations.mockResolvedValue(page([conv("k1", "55")]));
    api.conversationStatuses.mockResolvedValue({ items: [], next: undefined });
    api.markConversationRead.mockResolvedValue(conv("k1", "55"));
    api.deleteConversation.mockResolvedValue({});
  });
  afterEach(() => {
    useConversationsStore().stop();
    useAuthStore().setToken(null);
    vi.useRealTimers();
  });

  it("fetches the first page and subscribes to the user stream", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    expect(api.listConversations).toHaveBeenCalled();
    expect(store.items.map((c) => c.id)).toEqual(["k1"]);
    expect(stream.handler).not.toBeNull();
  });

  it("refetches on a direct update but ignores public updates", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    const calls = api.listConversations.mock.calls.length;

    stream.handler!({ event: "update", payload: { visibility: "public" } });
    await vi.runAllTimersAsync();
    expect(api.listConversations.mock.calls.length).toBe(calls);

    stream.handler!({ event: "update", payload: { visibility: "direct" } });
    await vi.runAllTimersAsync();
    expect(api.listConversations.mock.calls.length).toBeGreaterThan(calls);
  });

  it("refetches on delete only for a status it has loaded", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    const calls = api.listConversations.mock.calls.length;

    stream.handler!({ event: "delete", payload: "999" });
    await vi.runAllTimersAsync();
    expect(api.listConversations.mock.calls.length).toBe(calls);

    stream.handler!({ event: "delete", payload: "55" });
    await vi.runAllTimersAsync();
    expect(api.listConversations.mock.calls.length).toBeGreaterThan(calls);
  });

  it("live refresh merges instead of dropping loaded pages", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    // Simulate a second page already loaded.
    store.items.push(conv("k-old", "40"));
    api.listConversations.mockResolvedValue(
      page([conv("k1", "55"), conv("k2", "60")]),
    );

    stream.handler!({ event: "update", payload: { visibility: "direct" } });
    await vi.runAllTimersAsync();
    expect(store.items.map((c) => c.id)).toEqual(["k1", "k2", "k-old"]);
  });

  it("openThread marks the conversation read", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    await store.openThread("k1");
    expect(api.markConversationRead).toHaveBeenCalledWith("k1");
    expect(store.activeThread?.id).toBe("k1");
  });

  it("stop unsubscribes and clears state", async () => {
    const store = useConversationsStore();
    store.start();
    await vi.runAllTimersAsync();
    store.stop();
    expect(stream.unsubscribed).toBe(true);
    expect(store.items).toEqual([]);
    expect(store.activeThread).toBeNull();
  });
});
