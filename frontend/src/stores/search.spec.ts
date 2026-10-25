import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { ApiError } from "@/api/errors";

const api = vi.hoisted(() => ({ search: vi.fn() }));
vi.mock("@/api/search", () => api);

import { useSearchStore } from "./search";
import { useAuthStore } from "./auth";

const empty = { accounts: [], statuses: [], hashtags: [] };

describe("search store", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    useAuthStore().setToken("tok");
    api.search.mockResolvedValue({ ...empty, accounts: [{ id: "1" }] });
  });
  afterEach(() => {
    vi.useRealTimers();
    useAuthStore().setToken(null);
  });

  it("autocomplete never resolves remotely and splits by type", async () => {
    const store = useSearchStore();
    store.suggest("al", 0);
    await vi.runAllTimersAsync();
    expect(api.search).toHaveBeenCalledTimes(2);
    for (const call of api.search.mock.calls) {
      expect(call[0].resolve).toBe(false);
      expect(call[0].limit).toBe(5);
      expect(["accounts", "hashtags"]).toContain(call[0].type);
    }
  });

  it("ignores stale responses from an earlier keystroke", async () => {
    let release: (v: unknown) => void;
    api.search.mockImplementation(
      () => new Promise((r) => (release = r)),
    );
    const store = useSearchStore();
    store.suggest("one", 0);
    await vi.runAllTimersAsync();
    // Second query supersedes the first.
    api.search.mockResolvedValue({ ...empty });
    store.suggest("two", 0);
    await vi.runAllTimersAsync();
    release!({ ...empty, accounts: [{ id: "stale" }] });
    await vi.runAllTimersAsync();
    expect(
      store.suggestions?.accounts.some((a) => a.id === "stale"),
    ).toBe(false);
  });

  it("run rethrows so callers can distinguish a 401", async () => {
    api.search.mockRejectedValue(new ApiError("unauthorized", 401));
    const store = useSearchStore();
    const err = (await store.run("q").catch((e) => e)) as ApiError;
    expect(err.status).toBe(401);
    expect(store.error).toBe("unauthorized");
  });
});
