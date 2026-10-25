import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import ModerationReportsPage from "./ModerationReportsPage.vue";

const report = {
  id: "3",
  action_taken: false,
  action_taken_at: null,
  category: "spam",
  comment: "spam links",
  forwarded: false,
  created_at: "2026-01-02T03:04:05Z",
  account_id: "9",
  account: { id: "9", acct: "reporter" },
  target_account: { id: "4", acct: "bad" },
  status_id: "55",
  assigned_account: null,
};

function json(body: unknown) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

describe("ModerationReportsPage", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("lists open reports with reporter → target", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json([report]));
    const { wrapper } = mountWith(ModerationReportsPage);
    await flushPromises();

    expect(fetchSpy.mock.calls[0][0]).toContain("/api/v1/admin/reports");
    expect(wrapper.text()).toContain("#3");
    expect(wrapper.text()).toContain("Spam");
    expect(wrapper.text()).toContain("@reporter → @bad");
    expect(wrapper.find('a[href="/moderation/reports/3"]').exists()).toBe(true);
  });

  it("toggles resolved reports", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => json([]));
    const { wrapper } = mountWith(ModerationReportsPage);
    await flushPromises();
    await wrapper.get('input[type="checkbox"]').setValue(true);
    await flushPromises();
    expect(fetchSpy.mock.calls[1][0]).toContain("resolved=true");
  });
});
