import { beforeEach, describe, expect, it, vi } from "vitest";
import { useAuthStore } from "@/stores/auth";
import {
  exportDomainBlocksCsv,
  importDomainBlocksCsv,
} from "./adminDomainBlocks";

function mockFetch(status: number, body: unknown = {}) {
  return vi.spyOn(globalThis, "fetch").mockImplementation(() =>
    Promise.resolve(
      new Response(typeof body === "string" ? body : JSON.stringify(body), {
        status,
      }),
    ),
  );
}

describe("admin domain blocks csv api", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAuthStore().setToken(null);
  });

  it("exports the CSV with the bearer token", async () => {
    useAuthStore().setToken("tok");
    const spy = mockFetch(200, "csv");
    await exportDomainBlocksCsv();
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/admin/domain_blocks/export");
    expect(init!.method).toBe("GET");
    expect((init!.headers as Headers).get("Authorization")).toBe("Bearer tok");
  });

  it("imports multipart CSV and omits overwrite=false", async () => {
    const spy = mockFetch(200, { created: 1 });
    const file = new File(["bad.example\n"], "blocks.csv", {
      type: "text/csv",
    });
    await importDomainBlocksCsv(file);
    const [url, init] = spy.mock.calls[0];
    expect(url).toBe("/api/v1/admin/domain_blocks/import");
    expect(init!.method).toBe("POST");
    const form = init!.body as FormData;
    expect((form.get("file") as File).name).toBe("blocks.csv");
    expect(form.get("overwrite")).toBeNull();
  });

  it("sends overwrite=true only when requested", async () => {
    const spy = mockFetch(200, { updated: 1 });
    await importDomainBlocksCsv(new File(["x"], "blocks.csv"), true);
    const form = spy.mock.calls[0][1]!.body as FormData;
    expect(form.get("overwrite")).toBe("true");
  });
});
