import { nextTick } from "vue";
import { describe, expect, it, vi } from "vitest";
import { mount } from "@vue/test-utils";
import Tooltip from "./Tooltip.vue";

describe("Tooltip", () => {
  it("shows on focus with a describedby link", async () => {
    vi.useFakeTimers();
    const w = mount(Tooltip, {
      props: { text: "hint" },
      slots: { default: "<button>b</button>" },
      attachTo: document.body,
    });
    await w.trigger("focusin");
    vi.runAllTimers();
    await nextTick();
    const bubble = document.querySelector("[role='tooltip']");
    expect(bubble?.textContent).toBe("hint");
    expect(w.attributes("aria-describedby")).toBe(bubble?.id);
    vi.useRealTimers();
  });
});
