import { beforeEach, describe, expect, it, vi } from "vitest";
import { flushPromises, mount } from "@vue/test-utils";
import { createMemoryHistory, createRouter } from "vue-router";
import { i18n } from "@/i18n";
import { stubRoutes } from "@/test/render";
import AppLayout from "@/components/layout/AppLayout.vue";
import { useSearchStore } from "@/stores/search";
import SearchPage from "./SearchPage.vue";

const api = vi.hoisted(() => ({ search: vi.fn() }));
vi.mock("@/api/search", () => api);

const empty = { accounts: [], statuses: [], hashtags: [] };

function seedSuggestions(owner: string) {
  const store = useSearchStore();
  store.suggestions = {
    accounts: [
      {
        id: "1",
        acct: "alice",
        username: "alice",
        display_name: "Alice",
        avatar: "",
        emojis: [],
      },
    ] as never,
    hashtags: [{ name: "alice" }],
  };
  store.suggestionOwner = owner;
}

async function mountAt(path: string, attach = false) {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: stubRoutes,
  });
  await router.push(path);
  const wrapper = mount(SearchPage, {
    global: { plugins: [i18n, router] },
    ...(attach ? { attachTo: document.body } : {}),
  });
  return { wrapper, router };
}

describe("SearchPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.clearAllMocks();
    api.search.mockResolvedValue({ ...empty });
  });

  it("runs a new search when the route query changes while mounted", async () => {
    const { wrapper, router } = await mountAt("/search?q=first");
    await flushPromises();
    expect(api.search).toHaveBeenCalledWith(
      expect.objectContaining({ q: "first", limit: 40 }),
    );

    await router.push({ name: "search", query: { q: "second" } });
    await flushPromises();

    expect(
      (wrapper.get("input[type='search']").element as HTMLInputElement).value,
    ).toBe("second");
    expect(api.search).toHaveBeenLastCalledWith(
      expect.objectContaining({ q: "second", limit: 40 }),
    );
  });

  it("moves focus through suggestions with arrow keys or Tab", async () => {
    const { wrapper, router } = await mountAt("/search", true);
    seedSuggestions("page");
    await flushPromises();

    const input = wrapper.get("input[type='search']");
    const items = wrapper.findAll(".search__suggestion");
    expect(items).toHaveLength(2);

    (input.element as HTMLElement).focus();
    await input.trigger("keydown", { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[0].element);

    (input.element as HTMLElement).focus();
    await input.trigger("keydown", { key: "Tab" });
    expect(document.activeElement).toBe(items[0].element);

    await items[0].trigger("keydown", { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[1].element);
    await items[1].trigger("keydown", { key: "ArrowDown" });
    expect(document.activeElement).toBe(items[0].element);
    await items[0].trigger("keydown", { key: "ArrowUp" });
    expect(document.activeElement).toBe(items[1].element);

    await items[1].trigger("keydown", { key: "Enter" });
    await flushPromises();
    expect(router.currentRoute.value.path).toBe("/tags/alice");
    expect(api.search).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it("Enter in the input searches even while suggestions are visible", async () => {
    const { wrapper, router } = await mountAt("/search");
    seedSuggestions("page");
    await flushPromises();
    const input = wrapper.get("input[type='search']");
    await input.setValue("cats");
    await input.trigger("keydown", { key: "Enter" });
    await flushPromises();

    expect(router.currentRoute.value.query.q).toBe("cats");
    expect(api.search).toHaveBeenCalledWith(
      expect.objectContaining({ q: "cats", limit: 40 }),
    );
  });

  it("passes a layout search submission to the already-mounted page", async () => {
    vi.spyOn(globalThis, "fetch").mockImplementation(() =>
      Promise.resolve(new Response("{}", { status: 200 })),
    );
    const router = createRouter({
      history: createMemoryHistory(),
      routes: stubRoutes,
    });
    await router.push("/search?q=first");
    const layout = mount(AppLayout, { global: { plugins: [i18n, router] } });
    const page = mount(SearchPage, { global: { plugins: [i18n, router] } });
    await flushPromises();

    const input = layout.get(".layout__search--sidebar input");
    await input.setValue("second");
    await input.trigger("keydown", { key: "Enter" });
    await flushPromises();

    expect(router.currentRoute.value.query.q).toBe("second");
    expect(
      (page.get("input[type='search']").element as HTMLInputElement).value,
    ).toBe("second");
    expect(api.search).toHaveBeenLastCalledWith(
      expect.objectContaining({ q: "second", limit: 40 }),
    );
  });

  it("submitted in-page searches update the route query", async () => {
    const { wrapper, router } = await mountAt("/search");
    const input = wrapper.get("input[type='search']");
    await input.setValue("cats");
    await input.trigger("keydown", { key: "Enter" });
    await flushPromises();

    expect(router.currentRoute.value.query.q).toBe("cats");
    expect(api.search).toHaveBeenCalledWith(
      expect.objectContaining({ q: "cats", limit: 40 }),
    );
  });
});
