import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import AdminRelaysPage from "./AdminRelaysPage.vue";

const pendingRelay = {
  id: "1",
  inbox_url: "https://relay.example/inbox",
  state: "pending",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

function json(body: unknown) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

describe("AdminRelaysPage", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("lists relays with their state", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      json([pendingRelay]),
    );
    const { wrapper } = mountWith(AdminRelaysPage);
    await flushPromises();
    expect(wrapper.text()).toContain("relay.example/inbox");
    expect(wrapper.find(".relays__state").text()).toBe("Pending");
  });

  it("adds a relay and authorizes it", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementationOnce(() => json([]))
      .mockImplementationOnce(() => json(pendingRelay))
      .mockImplementationOnce(() =>
        json({ ...pendingRelay, state: "accepted" }),
      );
    const { wrapper } = mountWith(AdminRelaysPage);
    await flushPromises();

    await wrapper.find("input").setValue("https://relay.example/inbox");
    await wrapper.get("form").trigger("submit");
    await flushPromises();
    const [, init] = fetchSpy.mock.calls[1] as [string, RequestInit];
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({
      inbox_url: "https://relay.example/inbox",
    });

    await wrapper
      .findAll("button")
      .find((b) => b.text() === "Authorize")!
      .trigger("click");
    await flushPromises();
    const [url] = fetchSpy.mock.calls[2] as [string];
    expect(url).toContain("/admin/relays/1/authorize");
    expect(wrapper.find(".relays__state").text()).toBe("Accepted");
  });
});
