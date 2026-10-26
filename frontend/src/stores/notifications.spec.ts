import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { useNotificationsStore } from "./notifications";
import { useStreamingStore } from "./streaming";
import { useToastsStore } from "./toasts";
import { useAuthStore } from "./auth";

class FakeSocket {
  static instances: FakeSocket[] = [];
  readyState: number = WebSocket.CONNECTING;
  sent: string[] = [];
  url: string;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeSocket.instances.push(this);
  }
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.readyState = WebSocket.CLOSED;
    this.onclose?.();
  }
  open() {
    this.readyState = WebSocket.OPEN;
    this.onopen?.();
  }
  emit(frame: unknown) {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }
}

function latest(): FakeSocket {
  return FakeSocket.instances[FakeSocket.instances.length - 1];
}

class FakeNotification {
  static permission: NotificationPermission = "denied";
  static requestPermission = vi.fn(() =>
    Promise.resolve("granted" as NotificationPermission),
  );
  static instances: FakeNotification[] = [];
  title: string;
  options: NotificationOptions;
  onclick: (() => void) | null = null;
  closed = false;
  constructor(title: string, options?: NotificationOptions) {
    this.title = title;
    this.options = options ?? {};
    FakeNotification.instances.push(this);
  }
  close() {
    this.closed = true;
  }
}

function notif(id: string, type = "follow", extra: object = {}) {
  return JSON.stringify({
    id,
    type,
    created_at: "2026-10-26T00:00:00Z",
    account: {
      id: "9",
      acct: "bob@remote.example",
      display_name: "Bob Loblaw",
    },
    status: null,
    ...extra,
  });
}

describe("notifications store", () => {
  beforeEach(() => {
    FakeSocket.instances = [];
    FakeNotification.instances = [];
    FakeNotification.requestPermission.mockClear();
    vi.useFakeTimers();
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ count: 0 }), { status: 200 }),
      ),
    );
  });
  afterEach(() => {
    vi.useRealTimers();
    const s = useStreamingStore();
    s.disconnect();
    s.setSocketFactory(null);
    useNotificationsStore().stop();
    delete (globalThis as Record<string, unknown>).Notification;
    Object.defineProperty(document, "visibilityState", {
      value: "visible",
      configurable: true,
    });
  });

  function hiddenTab() {
    Object.defineProperty(document, "visibilityState", {
      value: "hidden",
      configurable: true,
    });
  }

  function start() {
    useAuthStore().setToken("tk");
    const streaming = useStreamingStore();
    streaming.setSocketFactory(
      (url) => new FakeSocket(url) as unknown as WebSocket,
    );
    const store = useNotificationsStore();
    store.start();
    latest().open();
    return store;
  }

  it("fetches the initial unread count and subscribes user:notification", async () => {
    vi.mocked(globalThis.fetch).mockImplementation(() =>
      Promise.resolve(
        new Response(JSON.stringify({ count: 4 }), { status: 200 }),
      ),
    );
    const store = start();
    expect(latest().sent).toContain(
      JSON.stringify({ type: "subscribe", stream: "user:notification" }),
    );
    await flushPromises();
    expect(store.unread).toBe(4);
  });

  it("notifications_count events set the badge without a refetch", async () => {
    const store = start();
    await flushPromises();
    const calls = vi.mocked(globalThis.fetch).mock.calls.length;
    latest().emit({
      stream: ["user:notification"],
      event: "notifications_count",
      payload: JSON.stringify({ unread_count: 7 }),
    });
    expect(store.unread).toBe(7);
    expect(vi.mocked(globalThis.fetch).mock.calls.length).toBe(calls);
  });

  it("live notifications prepend, dedupe and raise a toast", () => {
    const store = start();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("1"),
    });
    expect(store.items.map((n) => n.id)).toEqual(["1"]);
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("1"),
    });
    expect(store.items).toHaveLength(1);
    const toasts = useToastsStore();
    expect(toasts.items).toHaveLength(2);
    expect(toasts.items[0].text).toBe("Bob Loblaw followed you");
    expect(toasts.items[0].to).toBe("/@bob@remote.example");
  });

  it("status notifications toast-link to the post", () => {
    start();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("2", "mention", { status: { id: "55" } }),
    });
    const toasts = useToastsStore();
    expect(toasts.items[0].text).toBe("Bob Loblaw mentioned you");
    expect(toasts.items[0].to).toBe("/statuses/55");
  });

  it("filtered-out types still toast but skip the list", () => {
    const store = start();
    store.activeFilter = "mentions";
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("3", "favourite"),
    });
    expect(store.items).toHaveLength(0);
    expect(useToastsStore().items).toHaveLength(1);
  });

  it("fires a native notification when granted and the tab is hidden", () => {
    FakeNotification.permission = "granted";
    vi.stubGlobal("Notification", FakeNotification);
    hiddenTab();
    start();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("1", "mention", { status: { id: "55" } }),
    });
    expect(FakeNotification.instances).toHaveLength(1);
    const n = FakeNotification.instances[0];
    expect(n.title).toBe("Bob Loblaw mentioned you");
    expect(n.options.tag).toBe("yonder-notification-1");
    expect(n.options.data).toEqual({ url: "/statuses/55" });
  });

  it("skips the native notification while the tab is visible", () => {
    FakeNotification.permission = "granted";
    vi.stubGlobal("Notification", FakeNotification);
    start();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("1"),
    });
    expect(FakeNotification.instances).toHaveLength(0);
  });

  it("skips the native notification when permission is denied", () => {
    FakeNotification.permission = "denied";
    vi.stubGlobal("Notification", FakeNotification);
    hiddenTab();
    start();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("1"),
    });
    expect(FakeNotification.instances).toHaveLength(0);
  });

  it("requests permission once on the next user gesture", () => {
    FakeNotification.permission = "default";
    vi.stubGlobal("Notification", FakeNotification);
    start();
    expect(FakeNotification.requestPermission).not.toHaveBeenCalled();
    window.dispatchEvent(new Event("pointerdown"));
    window.dispatchEvent(new Event("keydown"));
    expect(FakeNotification.requestPermission).toHaveBeenCalledTimes(1);
  });

  it("never re-asks a denied permission", () => {
    FakeNotification.permission = "denied";
    vi.stubGlobal("Notification", FakeNotification);
    start();
    window.dispatchEvent(new Event("pointerdown"));
    expect(FakeNotification.requestPermission).not.toHaveBeenCalled();
  });

  it("markRead flips local rows, decrements the badge and posts ids", async () => {
    const store = start();
    await flushPromises();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("8"),
    });
    store.unread = 3;
    await store.markRead(["8"]);
    expect(store.items[0].read).toBe(true);
    expect(store.unread).toBe(2);
    const post = vi
      .mocked(globalThis.fetch)
      .mock.calls.find((c) => String(c[0]).includes("/notifications/read"));
    expect(post).toBeTruthy();
    expect(JSON.parse(post![1]!.body as string)).toEqual({ ids: [8] });
    // A second pass is a no-op — no extra request.
    const calls = vi.mocked(globalThis.fetch).mock.calls.length;
    await store.markRead(["8"]);
    expect(vi.mocked(globalThis.fetch).mock.calls.length).toBe(calls);
  });

  it("stop unsubscribes and clears state", async () => {
    const store = start();
    await flushPromises();
    latest().emit({
      stream: ["user:notification"],
      event: "notification",
      payload: notif("4"),
    });
    store.stop();
    expect(store.items).toHaveLength(0);
    expect(store.unread).toBe(0);
    expect(latest().sent).toContain(
      JSON.stringify({ type: "unsubscribe", stream: "user:notification" }),
    );
  });
});
