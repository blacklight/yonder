import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import Menu from "./Menu.vue";

const items = [
  { id: "a", label: "Alpha" },
  { id: "b", label: "Beta" },
  { id: "c", label: "Gamma", disabled: true },
];

describe("Menu", () => {
  it("opens on trigger click and selects an item", async () => {
    const w = mount(Menu, { props: { items, label: "menu" } });
    await w.get(".menu__trigger").trigger("click");
    expect(w.find("[role='menu']").exists()).toBe(true);
    await w.findAll("[role='menuitem']")[1].trigger("click");
    expect(w.emitted("select")?.[0]?.[0]).toMatchObject({ id: "b" });
  });
  it("navigates with arrows skipping disabled items", async () => {
    const w = mount(Menu, { props: { items, label: "menu" } });
    await w.get(".menu__trigger").trigger("click");
    await w.get("[role='menu']").trigger("keydown", { key: "ArrowDown" });
    await w.get("[role='menu']").trigger("keydown", { key: "ArrowDown" });
    // Second press wraps past disabled Gamma back to Alpha.
    await w.get("[role='menu']").trigger("keydown", { key: "Enter" });
    const emitted = w.emitted("select");
    expect(emitted).toBeTruthy();
  });
  it("opens on ArrowDown with focus moved into the list", async () => {
    const w = mount(Menu, {
      props: { items, label: "menu" },
      attachTo: document.body,
    });
    await w.get(".menu__trigger").trigger("keydown", { key: "ArrowDown" });
    const list = w.get("[role='menu']");
    expect(document.activeElement).toBe(list.element);
    // ArrowDown landed on the first enabled item.
    expect(list.attributes("aria-activedescendant")).toBe(
      w.findAll("[role='menuitem']")[0].attributes("id"),
    );
    w.unmount();
  });
  it("Escape closes and returns focus to the trigger", async () => {
    const w = mount(Menu, {
      props: { items, label: "menu" },
      attachTo: document.body,
    });
    await w.get(".menu__trigger").trigger("keydown", { key: "ArrowDown" });
    await w.get("[role='menu']").trigger("keydown", { key: "Escape" });
    expect(w.find("[role='menu']").exists()).toBe(false);
    expect(document.activeElement).toBe(w.get(".menu__trigger").element);
    w.unmount();
  });
  it("closes on outside pointerdown", async () => {
    const w = mount(Menu, {
      props: { items, label: "menu" },
      attachTo: document.body,
    });
    await w.get(".menu__trigger").trigger("click");
    document.body.dispatchEvent(new Event("pointerdown", { bubbles: true }));
    await w.vm.$nextTick();
    expect(w.find("[role='menu']").exists()).toBe(false);
    w.unmount();
  });
});
