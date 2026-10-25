import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import AdminAuditPage from "./AdminAuditPage.vue";

const row = (id: string) => ({
  id,
  actor_account_id: "1",
  actor_account: { id: "1", username: "root", domain: null },
  action: "config.set",
  target_type: "config",
  target_id: null,
  details: { key: "instance.name" },
  created_at: "2026-01-01T00:00:00Z",
});

function json(body: unknown, headers: Record<string, string> = {}) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json", ...headers },
    }),
  );
}

describe("AdminAuditPage", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("renders entries and follows the relative Link cursor", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() =>
        json([row("9")], {
          link: '</api/v1/admin/audit_log?max_id=8&limit=40>; rel="next"',
        }),
      )
      .mockImplementationOnce(() => json([row("8")], {}));
    const { wrapper } = mountWith(AdminAuditPage);
    await flushPromises();

    expect(wrapper.text()).toContain("config.set");
    expect(wrapper.text()).toContain("@root");

    await wrapper
      .findAll("button")
      .find((b) => b.text() === "Load more")!
      .trigger("click");
    await flushPromises();

    const [url] = fetchSpy.mock.calls[1] as [string];
    expect(url).toContain("max_id=8");
    expect(wrapper.findAll("tbody tr").length).toBe(2);
  });

  it("sends the filters as query params", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json([]));
    const { wrapper } = mountWith(AdminAuditPage);
    await flushPromises();
    await wrapper.findAll("input")[0].setValue("config.");
    await wrapper.get("form").trigger("submit");
    await flushPromises();
    const [url] = fetchSpy.mock.calls[1] as [string];
    expect(url).toContain("action=config.");
  });
});
