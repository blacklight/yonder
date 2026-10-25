import { describe, expect, it } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import { useSettingsStore } from "@/stores/settings";
import CwBlock from "./CwBlock.vue";

function mountBlock(props: { spoilerText: string }) {
  return mountWith(CwBlock, {
    props,
    slots: { default: '<p class="cw-body-test">the body</p>' },
  });
}

describe("CwBlock", () => {
  it("shows content openly when there is no warning", () => {
    const { wrapper } = mountBlock({ spoilerText: "" });
    expect(wrapper.find(".cw__header").exists()).toBe(false);
    expect(
      (wrapper.find(".cw__body").element as HTMLElement).style.display,
    ).not.toBe("none");
  });

  it("collapses behind the spoiler text", () => {
    const { wrapper } = mountBlock({ spoilerText: "gore" });
    expect(wrapper.get(".cw__text").text()).toBe("gore");
    expect(
      (wrapper.find(".cw__body").element as HTMLElement).style.display,
    ).toBe("none");
  });

  it("honours cw_always_expand even when settings arrive after mount", async () => {
    const settings = useSettingsStore();
    settings.settings = null;
    const { wrapper } = mountBlock({ spoilerText: "warn" });
    expect(
      (wrapper.find(".cw__body").element as HTMLElement).style.display,
    ).toBe("none");
    // Settings loading late still expands the block.
    settings.settings = { cw_always_expand: true } as never;
    await flushPromises();
    expect(
      (wrapper.find(".cw__body").element as HTMLElement).style.display,
    ).not.toBe("none");
  });
});
