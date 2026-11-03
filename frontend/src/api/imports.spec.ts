import { describe, expect, it, vi, beforeEach } from "vitest";
import { useAuthStore } from "@/stores/auth";
import {
  createImport,
  createImportSession,
  historyMedia,
  resumeImport,
} from "./settings";

function mockFetch(status: number, body: unknown = {}) {
  return vi
    .spyOn(globalThis, "fetch")
    .mockImplementation(() =>
      Promise.resolve(new Response(JSON.stringify(body), { status })),
    );
}

describe("imports api", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken(null);
  });

  it("createImport POSTs multipart with kind, mode, file", async () => {
    const spy = mockFetch(200, { id: "1" });
    const file = new File(["zip"], "export.zip");
    await createImport("mastodon", "merge", file);
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/yonder/imports");
    expect(init!.method).toBe("POST");
    const form = init!.body as FormData;
    expect(form).toBeInstanceOf(FormData);
    expect(form.get("kind")).toBe("mastodon");
    expect(form.get("mode")).toBe("merge");
    expect(form.get("federate")).toBeNull();
    expect((form.get("file") as File).name).toBe("export.zip");
  });

  it("createImport sends federate=true only when opted in", async () => {
    const spy = mockFetch(200, { id: "1" });
    const file = new File(["zip"], "export.zip");
    await createImport("mastodon", "overwrite", file, true);
    const form = spy.mock.calls[0][1]!.body as FormData;
    expect(form.get("federate")).toBe("true");
  });

  it("createImportSession POSTs the options with string ids", async () => {
    const spy = mockFetch(200, { id: "9", state: "pending" });
    await createImportSession({
      // Snowflake ids travel as strings — `Number()` corrupts them
      // past Number.MAX_SAFE_INTEGER.
      connection_id: "117519765199651274",
      categories: ["statuses", "observations"],
      limit: 500,
      order: "oldest",
      federate: true,
      overwrite_profile: true,
    });
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/yonder/import_sessions");
    expect(init!.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({
      connection_id: "117519765199651274",
      categories: ["statuses", "observations"],
      limit: 500,
      order: "oldest",
      federate: true,
      overwrite_profile: true,
    });
  });

  it("resumeImport keeps the connection id a decimal string", async () => {
    const spy = mockFetch(200, {});
    await resumeImport("44", "117519765199651274");
    expect(spy.mock.calls[0][0]).toBe("/api/v1/yonder/imports/44/resume");
    expect(JSON.parse(spy.mock.calls[0][1]!.body as string)).toEqual({
      connection_id: "117519765199651274",
    });
  });

  it("historyMedia GETs the owner-scoped media path as a blob", async () => {
    useAuthStore().setToken("tok");
    const spy = mockFetch(200);
    await historyMedia("117519765199651274", 2);
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe(
      "/api/v1/yonder/history/messages/117519765199651274/media/2",
    );
    expect((init!.headers as Headers).get("Authorization")).toBe("Bearer tok");
  });
});
