import { describe, expect, it, vi, beforeEach } from "vitest";
import { useAuthStore } from "@/stores/auth";
import { ApiError, requestBlob } from "./client";

function mockFetch(status: number, headers: Record<string, string> = {}) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockResolvedValue(new Response("bytes", { status, headers }));
}

describe("requestBlob", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken(null);
  });

  it("sends the bearer token and reads the filename", async () => {
    useAuthStore().setToken("tok");
    const spy = mockFetch(200, {
      "content-disposition": 'attachment; filename="export.csv"',
    });
    const { filename } = await requestBlob({
      path: "/yonder/exports/1/download",
    });
    const [, init] = spy.mock.calls[0];
    expect((init!.headers as Headers).get("Authorization")).toBe("Bearer tok");
    expect(filename).toBe("export.csv");
  });

  it("supports POST with a JSON body", async () => {
    useAuthStore().setToken("tok");
    const spy = mockFetch(200);
    await requestBlob({
      path: "/yonder/exports",
      method: "POST",
      body: { kind: "blocks" },
    });
    const [, init] = spy.mock.calls[0];
    expect(init!.method).toBe("POST");
    expect((init!.headers as Headers).get("Content-Type")).toBe(
      "application/json",
    );
    expect(init!.body).toBe(JSON.stringify({ kind: "blocks" }));
  });

  it("decodes filename* and strips any path components", async () => {
    mockFetch(200, {
      "content-disposition":
        "attachment; filename*=UTF-8''%2Ftmp%2Fexport%20list.csv",
    });
    const { filename } = await requestBlob({ path: "/x" });
    expect(filename).toBe("export list.csv");
  });

  it("falls back on a malformed filename escape", async () => {
    mockFetch(200, {
      "content-disposition": "attachment; filename*=UTF-8''%E0%A4%A",
    });
    const { filename } = await requestBlob({ path: "/x" });
    expect(filename).toBe("download");
  });

  it("returns the blob body", async () => {
    mockFetch(200);
    const { blob } = await requestBlob({ path: "/x" });
    expect(await blob.text()).toBe("bytes");
  });

  it("evicts the token on 401 and throws ApiError", async () => {
    useAuthStore().setToken("dead");
    mockFetch(401, { "content-type": "application/json" });
    const err = (await requestBlob({ path: "/x" }).catch((e) => e)) as ApiError;
    expect(err).toBeInstanceOf(ApiError);
    expect(useAuthStore().token).toBeNull();
  });
});
