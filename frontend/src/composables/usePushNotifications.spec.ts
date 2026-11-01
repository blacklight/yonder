import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { usePushNotifications } from "./usePushNotifications";
import {
  createPushSubscription,
  deletePushSubscription,
  getPushSubscription,
  getVapidPublicKey,
  updatePushSubscription,
} from "@/api/push";

vi.mock("@/api/push", () => ({
  createPushSubscription: vi.fn(),
  deletePushSubscription: vi.fn(),
  getPushSubscription: vi.fn(),
  getVapidPublicKey: vi.fn(),
  updatePushSubscription: vi.fn(),
}));

const mockedVapid = vi.mocked(getVapidPublicKey);
const mockedGet = vi.mocked(getPushSubscription);
const mockedCreate = vi.mocked(createPushSubscription);
const mockedDelete = vi.mocked(deletePushSubscription);
const mockedUpdate = vi.mocked(updatePushSubscription);

/** A fake browser PushSubscription in the shape `toJSON()` produces. */
function browserSub(endpoint = "https://push.example.com/send/1") {
  return {
    endpoint,
    unsubscribe: vi.fn(async () => true),
    toJSON: () => ({
      endpoint,
      keys: { p256dh: "cDI2NGRoLWtleQ", auth: "YXV0aC1rZXk" },
    }),
  } as unknown as PushSubscription;
}

const backendSub = {
  id: "1",
  endpoint: "https://push.example.com/send/1",
  alerts: { mention: true },
  policy: "all" as const,
  server_key: "server-key",
};

function stubEnv({
  browser = null as PushSubscription | null,
  permission = "default" as NotificationPermission,
  withSw = true,
  withPushManager = true,
} = {}) {
  const subscribe = vi.fn(async () => browserSub());
  const getSubscription = vi.fn(async () => browser);
  const registration = withSw
    ? ({
        pushManager: withPushManager ? { getSubscription, subscribe } : null,
      } as unknown as ServiceWorkerRegistration)
    : undefined;
  vi.stubGlobal("navigator", {
    serviceWorker: {
      getRegistration: vi.fn(async () => registration),
      ready: Promise.resolve(registration),
    },
  });
  vi.stubGlobal("PushManager", withPushManager ? class {} : undefined);
  vi.stubGlobal("Notification", { permission, requestPermission: vi.fn() });
  return { subscribe, getSubscription };
}

describe("usePushNotifications", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockedVapid.mockResolvedValue(
      "BEl62iUYgUivxIkv69yViEuiBIa-Ib9-SkvMeAtA3LFgDzkrxZJjSgSnfckjBJuBkr3qBUYIHBQFLXYp5Nksh8U",
    );
    mockedGet.mockResolvedValue(null);
    mockedCreate.mockResolvedValue(backendSub);
    mockedDelete.mockResolvedValue(undefined);
    mockedUpdate.mockResolvedValue(backendSub);
  });

  afterEach(() => vi.unstubAllGlobals());

  it("reports unsupported without service worker or PushManager", async () => {
    vi.stubGlobal("navigator", {});
    vi.stubGlobal("PushManager", undefined);
    const push = usePushNotifications();
    await push.load();
    expect(push.state.value).toBe("unsupported");
    expect(push.isSupported.value).toBe(false);
  });

  it("reports denied when notification permission is blocked", async () => {
    stubEnv({ permission: "denied" });
    const push = usePushNotifications();
    await push.load();
    expect(push.state.value).toBe("denied");
  });

  it("reports unavailable when the instance has no VAPID key", async () => {
    stubEnv();
    mockedVapid.mockResolvedValue(null);
    const push = usePushNotifications();
    await push.load();
    expect(push.state.value).toBe("unavailable");
  });

  it("prompts when nothing is subscribed yet", async () => {
    stubEnv();
    const push = usePushNotifications();
    await push.load();
    expect(push.state.value).toBe("prompt");
    expect(push.isSubscribed.value).toBe(false);
  });

  it("reports subscribed when browser and backend agree", async () => {
    stubEnv({ browser: browserSub() });
    mockedGet.mockResolvedValue(backendSub);
    const push = usePushNotifications();
    await push.load();
    expect(push.state.value).toBe("subscribed");
    expect(push.isSubscribed.value).toBe(true);
    expect(mockedCreate).toHaveBeenCalledWith(
      "https://push.example.com/send/1",
      { p256dh: "cDI2NGRoLWtleQ", auth: "YXV0aC1rZXk" },
    );
  });

  it("subscribes through the browser and registers with the backend", async () => {
    const env = stubEnv();
    const push = usePushNotifications();
    await push.load();
    await push.enable();

    expect(env.subscribe).toHaveBeenCalledWith({
      userVisibleOnly: true,
      applicationServerKey: expect.any(Uint8Array),
    });
    expect(mockedCreate).toHaveBeenCalled();
    expect(push.state.value).toBe("subscribed");
  });

  it("returns to prompt when the permission dialog is denied", async () => {
    stubEnv();
    vi.stubGlobal("Notification", {
      permission: "default",
      requestPermission: vi.fn(async () => "denied" as NotificationPermission),
    });
    const push = usePushNotifications();
    await push.load();
    await push.enable();
    expect(push.state.value).toBe("denied");
    expect(mockedCreate).not.toHaveBeenCalled();
  });

  it("unsubscribes in the browser and deletes the backend row", async () => {
    const sub = browserSub();
    stubEnv({ browser: sub });
    mockedGet.mockResolvedValue(backendSub);
    const push = usePushNotifications();
    await push.load();
    await push.disable();
    expect(sub.unsubscribe).toHaveBeenCalled();
    expect(mockedDelete).toHaveBeenCalledWith(
      "https://push.example.com/send/1",
    );
    expect(push.state.value).toBe("prompt");
  });

  it("patches alerts and policy through update", async () => {
    stubEnv({ browser: browserSub() });
    mockedGet.mockResolvedValue(backendSub);
    const push = usePushNotifications();
    await push.load();
    await push.update({ mention: false }, "followed");
    expect(mockedUpdate).toHaveBeenCalledWith({ mention: false }, "followed");
  });
});
