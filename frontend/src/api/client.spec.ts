import { describe, expect, it, vi, beforeEach } from "vitest";
import { useAuthStore } from "@/stores/auth";
import { ApiError, request } from "./client";

function mockFetch(
  status: number,
  body: unknown,
  headers: Record<string, string> = {},
) {
  return vi.spyOn(globalThis, "fetch").mockResolvedValue(
    new Response(typeof body === "string" ? body : JSON.stringify(body), {
      status,
      statusText: status === 200 ? "OK" : "Error",
      headers,
    }),
  );
}

describe("api client", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("attaches the bearer token", async () => {
    useAuthStore().setToken("tok123");
    const spy = mockFetch(200, { ok: true });
    await request({ path: "/instance" });
    const [, init] = spy.mock.calls[0];
    expect((init!.headers as Headers).get("Authorization")).toBe(
      "Bearer tok123",
    );
  });

  it("maps { error } bodies to ApiError", async () => {
    mockFetch(422, { error: "theme must be one of: system" });
    const err = (await request({
      path: "/yonder/settings",
      method: "PATCH",
      body: {},
    }).catch((e) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(err.message).toBe("theme must be one of: system");
    expect(err.status).toBe(422);
  });

  it("does not throw on a non-JSON error body", async () => {
    mockFetch(500, "<html>oops</html>", { "content-type": "text/html" });
    const err = (await request({ path: "/x" }).catch((e) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
  });

  it("honors abort signals", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation((_u, init) => {
      const signal = init?.signal as AbortSignal;
      return new Promise((_res, rej) => {
        signal.addEventListener("abort", () =>
          rej(new DOMException("aborted", "AbortError")),
        );
      });
    });
    const controller = new AbortController();
    const promise = request({ path: "/x", signal: controller.signal });
    controller.abort();
    await expect(promise).rejects.toThrow("aborted");
    expect(spy).toHaveBeenCalled();
  });
});
