import { afterEach, describe, expect, it, vi } from "vitest";
import { usePwa } from "./usePwa";

// The switch guard must probe a path the service worker does NOT
// precache — `/index.html` is in the precache manifest and would be
// answered by the SW itself, hiding `x-yonder-frontend`.
function stubEnv(frontend: string | null) {
  const unregister = vi.fn(async () => true);
  const cacheDelete = vi.fn(async () => true);
  const fetchMock = vi.fn(async () => ({
    headers: { get: () => frontend },
  }));
  const reload = vi.fn();
  vi.stubGlobal("navigator", {
    serviceWorker: { getRegistration: async () => ({ unregister }) },
  });
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("caches", {
    keys: async () => ["precache-v1"],
    delete: cacheDelete,
  });
  vi.stubGlobal("location", { reload });
  return { unregister, cacheDelete, fetchMock, reload };
}

describe("guardFrontendSwitch", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("probes a non-precached URL for the frontend marker", async () => {
    const env = stubEnv("yonder");
    await usePwa().guardFrontendSwitch();
    expect(env.fetchMock).toHaveBeenCalledWith("/frontend-check", {
      cache: "no-store",
    });
  });

  it("keeps the worker when the served frontend matches", async () => {
    const env = stubEnv("yonder");
    await usePwa().guardFrontendSwitch();
    expect(env.unregister).not.toHaveBeenCalled();
  });

  it("unregisters, clears caches and reloads on a frontend switch", async () => {
    const env = stubEnv("mastodon-fe");
    await usePwa().guardFrontendSwitch();
    expect(env.unregister).toHaveBeenCalled();
    expect(env.cacheDelete).toHaveBeenCalledWith("precache-v1");
    expect(env.reload).toHaveBeenCalled();
  });

  it("does nothing without a registration", async () => {
    vi.stubGlobal("navigator", {
      serviceWorker: { getRegistration: async () => undefined },
    });
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    await usePwa().guardFrontendSwitch();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("survives a failed probe without unregistering", async () => {
    const env = stubEnv("yonder");
    env.fetchMock.mockRejectedValue(new Error("offline"));
    await expect(usePwa().guardFrontendSwitch()).resolves.toBeUndefined();
    expect(env.unregister).not.toHaveBeenCalled();
    expect(env.reload).not.toHaveBeenCalled();
  });
});
