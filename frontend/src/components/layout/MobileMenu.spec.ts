import { describe, expect, it } from "vitest";
import { flushPromises } from "@vue/test-utils";
import { mountWith } from "@/test/render";
import MobileMenu from "./MobileMenu.vue";

const ITEMS =
  '<button data-a>A</button><a href="/x" data-b>B</a><button data-c>C</button>';

describe("MobileMenu", () => {
  it("moves focus into the dialog on open and restores it on close", async () => {
    const opener = document.createElement("button");
    document.body.appendChild(opener);
    opener.focus();
    const { wrapper } = mountWith(MobileMenu, {
      props: { open: true },
      slots: { default: ITEMS },
    });
    await flushPromises();
    const panel = document.body.querySelector(".mobile-menu__panel")!;
    expect(panel.contains(document.activeElement)).toBe(true);
    await wrapper.setProps({ open: false } as never);
    await flushPromises();
    expect(document.activeElement).toBe(opener);
    wrapper.unmount();
    opener.remove();
  });

  it("traps Tab at both ends of the dialog", async () => {
    const { wrapper } = mountWith(MobileMenu, {
      props: { open: true },
      slots: { default: ITEMS },
    });
    await flushPromises();
    const items = document.body.querySelectorAll<HTMLElement>(
      ".mobile-menu__panel button, .mobile-menu__panel a",
    );
    const first = items[0]; // the ✕ close button
    const last = items[items.length - 1]; // [data-c]
    // Tab on the last focusable wraps to the first.
    last.focus();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab" }));
    expect(document.activeElement).toBe(first);
    // Shift+Tab on the first wraps to the last.
    first.focus();
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Tab", shiftKey: true }),
    );
    expect(document.activeElement).toBe(last);
    wrapper.unmount();
  });

  it("closes on Escape", async () => {
    const { wrapper } = mountWith(MobileMenu, {
      props: { open: true },
      slots: { default: ITEMS },
    });
    await flushPromises();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(wrapper.emitted("close")).toBeTruthy();
    wrapper.unmount();
  });
});
