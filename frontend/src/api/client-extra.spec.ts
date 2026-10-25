import { describe, expect, it, vi, beforeEach } from "vitest";
import { useAuthStore } from "@/stores/auth";
import { request } from "./client";

function mockFetch(status: number, body: unknown) {
  return vi.spyOn(globalThis, "fetch").mockImplementation(() =>
    Promise.resolve(
      new Response(typeof body === "string" ? body : JSON.stringify(body), {
        status,
      }),
    ),
  );
}

describe("api client — epic 24 extensions", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken(null);
  });

  it("sends FormData without a JSON content-type", async () => {
    const spy = mockFetch(200, { id: "1" });
    const form = new FormData();
    form.set("file", new Blob(["x"], { type: "image/png" }), "x.png");
    await request({ path: "/api/v2/media", method: "POST", body: form });
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v2/media");
    expect((init!.headers as Headers).get("Content-Type")).toBeNull();
    expect(init!.body).toBeInstanceOf(FormData);
  });

  it("serializes array query values as repeated params", async () => {
    const spy = mockFetch(200, []);
    await request({
      path: "/notifications",
      query: { "types[]": ["mention", "follow"], limit: 20 },
    });
    const [url] = spy.mock.calls[0];
    expect(url).toBe(
      "/api/v1/notifications?types%5B%5D=mention&types%5B%5D=follow&limit=20",
    );
  });

  it("passes extra headers (Idempotency-Key)", async () => {
    const spy = mockFetch(200, {});
    await request({
      path: "/statuses",
      method: "POST",
      body: { status: "hi" },
      headers: { "Idempotency-Key": "uuid-1" },
    });
    const [, init] = spy.mock.calls[0];
    expect((init!.headers as Headers).get("Idempotency-Key")).toBe("uuid-1");
  });

  it("clears the token on an authenticated 401", async () => {
    useAuthStore().setToken("dead");
    mockFetch(401, { error: "unauthorized" });
    await expect(request({ path: "/instance" })).rejects.toBeTruthy();
    expect(useAuthStore().token).toBeNull();
    expect(localStorage.getItem("yonder.token")).toBeNull();
  });

  it("keeps a newer token when a stale request's 401 lands", async () => {
    const auth = useAuthStore();
    auth.setToken("old");
    let release: (r: Response) => void;
    const gate = new Promise<Response>((res) => (release = res));
    vi.spyOn(globalThis, "fetch").mockImplementation(() => gate);
    const pending = request({ path: "/instance" });
    // The user re-signed in while the request was in flight.
    auth.setToken("new");
    release!(
      new Response(JSON.stringify({ error: "unauth" }), { status: 401 }),
    );
    await expect(pending).rejects.toBeTruthy();
    expect(auth.token).toBe("new");
  });

  it("keeps the token on a 401 that carried none", async () => {
    useAuthStore().setToken("alive");
    mockFetch(401, { error: "unauthorized" });
    // A request sent before the token existed must not clear it.
    await expect(
      request({ path: "/instance", skipAuth: true }),
    ).rejects.toBeTruthy();
    expect(useAuthStore().token).toBe("alive");
  });
});
