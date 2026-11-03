import { beforeEach, describe, expect, it, vi } from "vitest";
import { registerRoute } from "workbox-routing";

// `src/sw.ts` is a service-worker module: importing it registers
// listeners on the global `self`. Workbox is mocked out; `self` is
// replaced with a fake worker scope that records listeners.
vi.mock("workbox-core", () => ({ clientsClaim: vi.fn() }));
vi.mock("workbox-expiration", () => ({ ExpirationPlugin: vi.fn() }));
vi.mock("workbox-precaching", () => ({
  cleanupOutdatedCaches: vi.fn(),
  createHandlerBoundToURL: vi.fn(() => ({})),
  precacheAndRoute: vi.fn(),
}));
vi.mock("workbox-routing", () => ({
  NavigationRoute: vi.fn(),
  registerRoute: vi.fn(),
}));
vi.mock("workbox-strategies", () => ({ CacheFirst: vi.fn() }));

type Handler = (event: never) => void;

interface FakeSelf {
  __WB_MANIFEST: unknown[];
  listeners: Record<string, Handler>;
  location: { origin: string };
  registration: { showNotification: ReturnType<typeof vi.fn> };
  skipWaiting: ReturnType<typeof vi.fn>;
  clients: {
    matchAll: ReturnType<typeof vi.fn>;
    openWindow: ReturnType<typeof vi.fn>;
  };
  addEventListener: (name: string, handler: Handler) => void;
}

function fakeSelf(): FakeSelf {
  const self: FakeSelf = {
    __WB_MANIFEST: [],
    listeners: {},
    location: { origin: "https://yonder.example" },
    registration: { showNotification: vi.fn(async () => undefined) },
    skipWaiting: vi.fn(async () => undefined),
    clients: {
      matchAll: vi.fn(async () => []),
      openWindow: vi.fn(async () => null),
    },
    addEventListener(name: string, handler: Handler) {
      self.listeners[name] = handler;
    },
  };
  return self;
}

async function loadWorker() {
  const self = fakeSelf();
  vi.stubGlobal("self", self);
  vi.resetModules();
  await import("./sw");
  return self;
}

function pushEvent(payload: unknown) {
  let waited: Promise<unknown> | undefined;
  return {
    data: { json: () => payload },
    waitUntil: (p: Promise<unknown>) => {
      waited = p;
    },
    waited: () => waited,
  };
}

describe("service worker", () => {
  beforeEach(() => vi.unstubAllGlobals());

  it("shows a notification from the push payload", async () => {
    const self = await loadWorker();
    const event = pushEvent({
      title: "alice mentioned you",
      body: "",
      icon: "https://yonder.example/media/avatar.png",
      tag: "yonder-notification-42",
      data: { url: "/statuses/42" },
    });
    self.listeners.push(event as never);
    await event.waited();
    expect(self.registration.showNotification).toHaveBeenCalledWith(
      "alice mentioned you",
      expect.objectContaining({
        tag: "yonder-notification-42",
        icon: "https://yonder.example/media/avatar.png",
        data: { url: "/statuses/42" },
      }),
    );
  });

  it("falls back to safe defaults on an unparseable payload", async () => {
    const self = await loadWorker();
    const event = {
      data: { json: () => "not-json" },
      waited: undefined as Promise<unknown> | undefined,
      waitUntil(p: Promise<unknown>) {
        this.waited = p;
      },
    };
    self.listeners.push(event as never);
    await event.waited;
    expect(self.registration.showNotification).toHaveBeenCalledWith(
      "Yonder",
      expect.objectContaining({ tag: "yonder-notification" }),
    );
  });

  it("focuses an open app window on notification click", async () => {
    const self = await loadWorker();
    const focus = vi.fn(async () => undefined);
    const navigate = vi.fn(async () => undefined);
    self.clients.matchAll.mockResolvedValue([
      { url: "https://yonder.example/home", focus, navigate },
    ]);
    let waited: Promise<unknown> | undefined;
    const event = {
      notification: {
        close: vi.fn(),
        data: { url: "/statuses/7" },
      },
      waitUntil: (p: Promise<unknown>) => {
        waited = p;
      },
    };
    self.listeners.notificationclick(event as never);
    await waited;
    expect(focus).toHaveBeenCalled();
    expect(navigate).toHaveBeenCalledWith("/statuses/7");
    expect(self.clients.openWindow).not.toHaveBeenCalled();
  });

  it("opens a window when none is open", async () => {
    const self = await loadWorker();
    let waited: Promise<unknown> | undefined;
    const event = {
      notification: { close: vi.fn(), data: { url: "/notifications" } },
      waitUntil: (p: Promise<unknown>) => {
        waited = p;
      },
    };
    self.listeners.notificationclick(event as never);
    await waited;
    expect(self.clients.openWindow).toHaveBeenCalledWith("/notifications");
  });

  it("only caches same-origin images", async () => {
    await loadWorker();
    const matcher = vi
      .mocked(registerRoute)
      .mock.calls
      .map(([capture]) => capture)
      .find((c) => typeof c === "function");
    expect(matcher).toBeDefined();
    const match = (url: string, destination = "image") =>
      matcher?.({
        url: new URL(url),
        request: { destination },
      } as never);
    expect(match("https://yonder.example/media/avatar.png")).toBe(true);
    expect(match("https://media.remote.example/avatar.png")).toBe(false);
    expect(match("https://yonder.example/app.css", "style")).toBe(false);
  });

  it("never navigates a click off-origin", async () => {
    const self = await loadWorker();
    let waited: Promise<unknown> | undefined;
    const event = {
      notification: {
        close: vi.fn(),
        data: { url: "https://evil.example/phish" },
      },
      waitUntil: (p: Promise<unknown>) => {
        waited = p;
      },
    };
    self.listeners.notificationclick(event as never);
    await waited;
    expect(self.clients.openWindow).toHaveBeenCalledWith("/");
  });
});
