import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import { i18n } from "@/i18n";
import Modal from "./Modal.vue";

const mountModal = (props: { title: string }) =>
  mount(Modal, { props, global: { plugins: [i18n] } });

describe("Modal", () => {
  it("teleports a labelled dialog to body", () => {
    mountModal({ title: "Confirm" });
    const dialog = document.querySelector("[role='dialog']");
    expect(dialog).toBeTruthy();
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    expect(dialog?.getAttribute("aria-label")).toBe("Confirm");
  });
  it("closes on Escape", async () => {
    const w = mountModal({ title: "x" });
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(w.emitted("close")).toBeTruthy();
  });
  it("restores focus on unmount", async () => {
    const outside = document.createElement("button");
    document.body.appendChild(outside);
    outside.focus();
    const w = mountModal({ title: "x" });
    w.unmount();
    expect(document.activeElement).toBe(outside);
  });
});
