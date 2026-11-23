import { describe, expect, it, vi, beforeEach } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { createPinia, setActivePinia } from "pinia";
import EventsDiscoverPage from "./EventsDiscoverPage.vue";
import { useAuthStore } from "@/stores/auth";
import type { Status } from "@/api/entities";

const listEventsMock = vi.fn();
vi.mock("@/api/events", () => ({
  listEvents: (...a: unknown[]) => listEventsMock(...a),
  cursorOf: (url?: string) =>
    url
      ? (new URL(url, location.origin).searchParams.get("cursor") ?? undefined)
      : undefined,
  FEP_CATEGORIES: ["ARTS", "MUSIC", "SPORTS"],
}));
vi.mock("@/components/status/StatusCard.vue", () => ({
  default: {
    props: ["status"],
    template: "<div class='stub-card' :data-id='status.id' />",
  },
}));

function status(id: string): Status {
  return {
    id,
    account: { acct: "alice", id: "1" },
  } as unknown as Status;
}

async function mountPage(authed: boolean) {
  const pinia = createPinia();
  setActivePinia(pinia);
  useAuthStore().token = authed ? "test-token" : null;
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: "/", component: { template: "<div/>" } },
      { path: "/events", component: { template: "<div/>" } },
    ],
  });
  const wrapper = mount(EventsDiscoverPage, {
    global: { plugins: [i18n, router, pinia] },
  });
  await flushPromises();
  return wrapper;
}

function buttonByText(wrapper: ReturnType<typeof mount>, text: string) {
  const b = wrapper.findAll("button").find((b) => b.text() === text);
  expect(b, `button ${text}`).toBeTruthy();
  return b!;
}

describe("EventsDiscoverPage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    listEventsMock.mockResolvedValue({
      items: [status("1"), status("2")],
      next: undefined,
    });
  });

  it("anonymous visitors get local/federated tabs and default to local", async () => {
    const wrapper = await mountPage(false);
    const tabs = wrapper.findAll("[role='tab']").map((t) => t.text());
    expect(tabs).toHaveLength(2);
    expect(tabs.join(" ")).not.toMatch(/Upcoming/);
    expect(listEventsMock).toHaveBeenCalledWith(
      expect.objectContaining({ scope: "local" }),
    );
    expect(wrapper.findAll(".stub-card")).toHaveLength(2);
  });

  it("authenticated visitors get the home tab and can switch scopes", async () => {
    const wrapper = await mountPage(true);
    expect(wrapper.findAll("[role='tab']")).toHaveLength(3);
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ scope: "home" }),
    );
    await wrapper.findAll("[role='tab']")[2].trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ scope: "federated" }),
    );
  });

  it("renders the scope's own results and resets on tab switch", async () => {
    // Each scope returns a distinct set — a scope bug (e.g. federated
    // answering with local data) shows up as the wrong ids rendered.
    listEventsMock.mockImplementation(({ scope }: { scope: string }) =>
      Promise.resolve({
        items: [status(`${scope}-1`), status(`${scope}-2`)],
        next: undefined,
      }),
    );
    const wrapper = await mountPage(true);
    const ids = () =>
      wrapper.findAll(".stub-card").map((c) => c.attributes("data-id"));
    expect(ids()).toEqual(["home-1", "home-2"]);
    await buttonByText(wrapper, "Federated").trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ scope: "federated" }),
    );
    expect(ids()).toEqual(["federated-1", "federated-2"]);
  });

  it("drops a page that resolves after the tab changed", async () => {
    let resolveLocal!: (v: unknown) => void;
    listEventsMock.mockImplementationOnce(
      () => new Promise((resolve) => (resolveLocal = resolve)),
    );
    const wrapper = await mountPage(true);
    listEventsMock.mockImplementation(({ scope }: { scope: string }) =>
      Promise.resolve({ items: [status(`${scope}-1`)], next: undefined }),
    );
    await buttonByText(wrapper, "Federated").trigger("click");
    await flushPromises();
    // The slow local request finishes last — it must not overwrite
    // the federated results the user is looking at.
    resolveLocal({ items: [status("local-1")], next: undefined });
    await flushPromises();
    expect(
      wrapper.findAll(".stub-card").map((c) => c.attributes("data-id")),
    ).toEqual(["federated-1"]);
  });

  it("sends the category/tag/online filters and near-me coordinates", async () => {
    const wrapper = await mountPage(true);
    await wrapper.find("select").setValue("MUSIC");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ category: "MUSIC" }),
    );

    await wrapper.find("input[type='search']").setValue("yonder");
    await wrapper.find("input[type='search']").trigger("keydown.enter");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ tag: "yonder" }),
    );

    await wrapper.find("input[type='checkbox']").setValue(true);
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ online: true }),
    );
  });

  it("refilters the tag on blur only when it changed", async () => {
    const wrapper = await mountPage(true);
    const input = wrapper.find("input[type='search']");
    await input.setValue("running");
    await input.trigger("blur");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ tag: "running" }),
    );
    const calls = listEventsMock.mock.calls.length;
    // Blur without a change must not refilter.
    await input.trigger("blur");
    await flushPromises();
    expect(listEventsMock.mock.calls.length).toBe(calls);
  });

  it("sends near-me coordinates from browser geolocation and clears them", async () => {
    const getCurrentPosition = vi.fn((ok: PositionCallback) =>
      ok({
        coords: { latitude: 45.12345, longitude: 9.6789 },
        timestamp: 0,
      } as GeolocationPosition),
    );
    Object.defineProperty(navigator, "geolocation", {
      configurable: true,
      value: { getCurrentPosition },
    });
    const wrapper = await mountPage(true);
    await buttonByText(wrapper, "Near me").trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ near: "45.12345,9.67890" }),
    );
    expect(wrapper.text()).toContain("Showing events near you");
    // Clearing removes the filter.
    await buttonByText(wrapper, "Clear").trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ near: undefined }),
    );
  });

  it("shows the near-me failure state when geolocation is unavailable", async () => {
    Object.defineProperty(navigator, "geolocation", {
      configurable: true,
      value: undefined,
    });
    const wrapper = await mountPage(true);
    await buttonByText(wrapper, "Near me").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("Location unavailable");
  });

  it("surfaces load errors, drops the cursor and offers retry", async () => {
    listEventsMock.mockRejectedValueOnce(new Error("boom"));
    const wrapper = await mountPage(true);
    await flushPromises();
    expect(wrapper.text()).toContain("Something went wrong.");
    // Retry succeeds and re-requests page one (cursor was dropped).
    await buttonByText(wrapper, "Retry").trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ cursor: undefined }),
    );
  });

  it("retry after a paging failure replaces the list instead of duplicating it", async () => {
    listEventsMock.mockResolvedValueOnce({
      items: [status("1"), status("2")],
      next: "/api/v1/yonder/events?cursor=abc",
    });
    const wrapper = await mountPage(true);
    expect(wrapper.findAll(".stub-card")).toHaveLength(2);

    // Page two fails: the cursor is dropped so the sentinel stops.
    listEventsMock.mockRejectedValueOnce(new Error("boom"));
    await buttonByText(wrapper, "Load more").trigger("click");
    await flushPromises();
    expect(wrapper.text()).toContain("Something went wrong.");

    // Retry re-requests page one and *replaces* the two existing
    // cards — appending here would render four.
    listEventsMock.mockResolvedValueOnce({
      items: [status("1"), status("2")],
      next: undefined,
    });
    await buttonByText(wrapper, "Retry").trigger("click");
    await flushPromises();
    expect(listEventsMock).toHaveBeenLastCalledWith(
      expect.objectContaining({ cursor: undefined }),
    );
    expect(
      wrapper.findAll(".stub-card").map((c) => c.attributes("data-id")),
    ).toEqual(["1", "2"]);
  });
});
