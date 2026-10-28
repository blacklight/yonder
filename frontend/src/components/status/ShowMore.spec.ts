import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import ShowMore from "./ShowMore.vue";

// jsdom reports every scrollHeight as 0 — drive it per test and stub
// ResizeObserver (absent in jsdom) so the mount-time measure runs.
let scrollHeight = 0;

beforeEach(() => {
  scrollHeight = 0;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(
    () => scrollHeight,
  );
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

// `measure` runs in onMounted — the reactive update it triggers only
// lands in the DOM on the next flush.
const mountBody = async (disabled?: boolean) => {
  const mounted = mountWith(ShowMore, {
    props: { disabled },
    slots: { default: "<p>body</p>" },
  });
  await flushPromises();
  return mounted;
};

describe("ShowMore", () => {
  it("leaves short bodies alone — no toggle", async () => {
    scrollHeight = 100;
    const { wrapper } = await mountBody();
    expect(wrapper.find(".show-more__toggle").exists()).toBe(false);
    expect(wrapper.classes()).not.toContain("show-more--collapsed");
  });

  it("collapses tall bodies behind the fade and expands on click", async () => {
    scrollHeight = 1000;
    const { wrapper } = await mountBody();
    const toggle = wrapper.get(".show-more__toggle");
    expect(wrapper.classes()).toContain("show-more--collapsed");
    expect(toggle.text()).toBe("Show more");
    expect(toggle.attributes("aria-expanded")).toBe("false");

    await toggle.trigger("click");
    await flushPromises();
    expect(wrapper.classes()).not.toContain("show-more--collapsed");
    expect(wrapper.get(".show-more__toggle").text()).toBe("Show less");

    // And it re-collapses.
    await wrapper.get(".show-more__toggle").trigger("click");
    await flushPromises();
    expect(wrapper.classes()).toContain("show-more--collapsed");
  });

  it("never collapses when disabled", async () => {
    scrollHeight = 1000;
    const { wrapper } = await mountBody(true);
    expect(wrapper.find(".show-more__toggle").exists()).toBe(false);
    expect(wrapper.classes()).not.toContain("show-more--collapsed");
  });
});
